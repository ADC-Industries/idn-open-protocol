//! The switcher's state (`GET /api/v1/state`), declared: typed down to the parts a UI
//! reads most (inputs and their sources, scenes' layers, buses, transitions, mixes,
//! streams, problems), the rest a JSON value as the engine and the service make it.
//! Its schema is `GET /api/v1/state_schema`; a test reads a state into [`SwitcherState`]
//! and back and checks nothing was left out.
//!
//! The types read a state as the API sends it (`serde_json::from_value`), and keep working
//! when keys are added: an unknown key is skipped.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The switcher's state.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SwitcherState {
    /// Moves with every change (the engine's, the show's and undo's).
    pub version: u64,
    pub name: String,
    /// The output format.
    pub format: Format,
    /// The format from the next start (`format`), when another; null otherwise.
    pub format_next: Option<String>,
    /// The inputs: scenes, sources, colours, pictures, titles, …
    pub inputs: Vec<Input>,
    /// The scenes on program and preview (input ids; null: nothing).
    pub program: Option<u32>,
    pub preview: Option<u32>,
    /// The next transition.
    pub transition: Transition,
    /// Fade to black.
    pub ftb: Ftb,
    /// Layers placed in scenes and changed together (the OBS model): their definitions.
    pub global_layers: Vec<Layer>,
    /// The other mixes, each with its own buses (or following the program's).
    pub mixes: Vec<Mix>,
    pub multiview: Multiview,
    pub monitor: Monitor,
    /// The sound: its source, buses, inputs heard, delay, the desk link.
    pub audio: Value,
    pub captions: Captions,
    /// The stinger slots (1–8; null: empty).
    pub stingers: Vec<Option<Stinger>>,
    pub timers: Vec<Value>,
    /// Triggers (MQTT, timecode, …) and what they do.
    pub triggers: Vec<Value>,
    /// Sources on the network an input can take.
    pub network_sources: Vec<Value>,
    /// IDN Data's feeds, for a title's data (§S.20).
    pub data_feeds: Vec<Value>,
    /// The desks' feeds taken (§S.26), by this switcher's output.
    pub feeds: Vec<Feed>,
    pub receivers: u64,
    /// How frames are composed: `gpu` or `cpu`.
    pub compositor: String,
    /// The HDR path in use.
    pub hdr_path: String,
    /// A frame's time, and its worst over 10 s, ms; its parts; the GPU's.
    pub frame_ms: f64,
    pub frame_ms_worst_10s: f64,
    pub frame_parts_ms: Value,
    pub gpu_ms: Value,
    pub scene_gpu_ms: Value,
    pub late_frames: u64,
    /// The latest a frame started after its time, the worst over 10 s, ms.
    pub start_late_ms_worst_10s: f64,
    pub clock_alive: bool,
    pub clock_restarts: u64,
    /// Streams and recordings.
    pub streams: Vec<Stream>,
    /// The program's CPU use since the last look, % of one core.
    pub cpu_percent: f64,
    /// Its GPU use since the last look, % of the busiest engine; null where the system
    /// doesn't say.
    pub gpu_percent: Option<f64>,
    /// Saved destinations, without their keys.
    pub destinations: Vec<Value>,
    /// Encoders running for the streams and recordings (with FFmpeg).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encodes: Option<u64>,
    /// MQTT (§S.33): the site broker's link and topics.
    pub mqtt: Value,
    /// The starters built in.
    pub starters: Vec<Value>,
    pub stream_alerts: Vec<Value>,
    /// What is wrong now (§S.37).
    pub problems: Vec<Problem>,
    /// Media players.
    pub media: Value,
    /// Screens this machine shows outputs on (the display service's).
    pub screens: Vec<Value>,
    /// Saved setups and the current one.
    pub setups: Value,
    /// What undo and redo would do.
    pub undo: Undo,
    /// The show (cue list), its status, saved shows, desks a cue can recall on.
    pub show: Value,
}

/// The output format.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Format {
    pub width: u32,
    pub height: u32,
    /// Frames a second as a fraction: [numerator, denominator].
    pub fps: (u32, u32),
}

/// An input.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Input {
    pub id: u32,
    pub label: String,
    /// What it is: one of the keys set.
    pub source: InputSource,
    pub connected: bool,
    /// Receiving now (in use, or kept active).
    pub active: bool,
    pub keep_active: bool,
    /// A scene's desk snapshot recalled as it goes live: `{desk, snapshot}`.
    pub on_air_snapshot: Option<Value>,
    /// Its newest picture's size (0 × 0: none yet) and frame rate.
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// Its sound's sync offset, ms.
    pub audio_offset_ms: i64,
    /// A PTZ camera's control, last preset and error.
    pub camera: Option<Value>,
    pub ptz_capable: bool,
    /// How its pictures are decoded: `cpu`, `gpu`, `gpu-reconstruct` or "".
    pub decode: String,
    /// The receiver's last error (30 s).
    pub error: Option<String>,
    /// Lost while in use: why; and the attempts to reach it since.
    pub lost: Option<String>,
    pub attempts: u64,
    /// A title's data: row, rows, columns, advancing, error, read.
    pub data: Option<Value>,
    /// Its filters, in order.
    pub filters: Vec<Value>,
    /// Tally: shown on program (on air) or on preview, itself or in a scene.
    pub program: bool,
    pub preview: bool,
    /// A title's text in one line, for lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// What an input is: exactly one of these.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct InputSource {
    /// A source on the network, by its name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    /// `#rrggbb`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub colour: Option<String>,
    /// A picture file's path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picture: Option<String>,
    /// A feed from another switcher.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feed: Option<bool>,
    /// Another input delayed: `{input, ms}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scene: Option<Scene>,
    /// A PTZ camera's preset as a source: `{camera, preset}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ptz: Option<Value>,
    /// A virtual shot of another input: `{input, view, to, moving, crop, to_crop}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#virtual: Option<Value>,
    /// A title: its layers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<Value>,
    /// A slideshow: its pictures, timing and where it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slideshow: Option<Value>,
}

/// A scene: its layers, bottom to top.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Scene {
    pub items: Vec<Layer>,
}

/// A scene's layer (or a global layer's definition), as `scene_items` takes it.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Layer {
    /// Stable while it stays in its scene.
    pub id: u32,
    pub name: String,
    /// Volunteers may show and hide it (for the UI).
    pub volunteer: bool,
    /// Locked in the scene editor (for the UI).
    pub locked: bool,
    /// Seconds it fades over when shown or hidden.
    pub fade: f64,
    /// The global layer this is a copy of.
    pub global: Option<u32>,
    /// An input id, or `{"colour": "#rrggbb"}`.
    pub source: Value,
    /// x, y, width, height as fractions of the frame.
    pub rect: [f64; 4],
    /// x0, y0, x1, y1 as fractions of the source.
    pub crop: [f64; 4],
    /// Degrees clockwise.
    pub rotation: f64,
    pub opacity: f64,
    /// `normal`, `add`, `multiply` or `screen`.
    pub blend: String,
    /// Border width, px, and colour.
    pub border: u32,
    pub border_colour: String,
    /// Corner radius, a fraction of the shorter side (0 to 0.5).
    pub radius: f64,
    /// `alpha` (keyed by its source's alpha) or `none` (drawn opaque).
    pub key: String,
    pub visible: bool,
    /// Its input's sound in the scene, dB, and muted.
    pub volume_db: f64,
    pub muted: bool,
}

impl From<&crate::scene::Item> for Layer {
    fn from(it: &crate::scene::Item) -> Layer {
        use crate::scene::ItemSource;
        let four = |r: (f32, f32, f32, f32)| [r.0 as f64, r.1 as f64, r.2 as f64, r.3 as f64];
        Layer {
            id: it.id,
            name: it.name.clone(),
            volunteer: it.volunteer,
            locked: it.locked,
            fade: it.fade as f64,
            global: it.global,
            source: match &it.source {
                ItemSource::Input(id) => Value::from(*id),
                ItemSource::Colour(c) => serde_json::json!({"colour": c.to_hex()}),
            },
            rect: four(it.rect),
            crop: four(it.crop),
            rotation: it.rotation as f64,
            opacity: it.opacity as f64,
            blend: it.blend.name().to_owned(),
            border: it.border,
            border_colour: it.border_colour.to_hex(),
            radius: it.radius as f64,
            key: if it.keyed { "alpha" } else { "none" }.to_owned(),
            visible: it.visible,
            volume_db: it.volume_db as f64,
            muted: it.muted,
        }
    }
}

/// A transition (the program's or a mix's).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Transition {
    /// `cut`, `mix`, `dip`, a wipe, `luma`, `stinger1`…
    pub kind: String,
    /// Seconds.
    pub duration: f64,
    /// Where it is, 0–1 (the T-bar).
    pub position: f64,
    pub running: bool,
    /// A wipe's soft edge.
    pub soft: Option<f64>,
    /// A luma wipe's pattern input, soft edge and direction.
    pub luma: Option<Value>,
}

/// Fade to black.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Ftb {
    pub on: bool,
    /// 0 (none) to 1 (black).
    pub level: f64,
}

/// A mix: another program with its own buses (or the program's, in another shape).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Mix {
    pub id: u32,
    pub name: String,
    /// Its slot, from 1.
    pub slot: u32,
    pub width: u32,
    pub height: u32,
    pub program: Option<u32>,
    pub preview: Option<u32>,
    /// It takes the program's buses and transition (a vertical version of the program).
    pub follows_program: bool,
    pub transition: Transition,
}

/// The multiview.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Multiview {
    /// Its tiles: `{kind, id, rect, label, tally}`.
    pub tiles: Vec<Value>,
    /// Sent as a source of its own.
    pub output: bool,
    /// A layout of its own (not the default one).
    pub custom: bool,
}

/// The operator's monitor.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Monitor {
    /// Preview's sound instead of program's.
    pub preview: bool,
    /// Inputs soloed.
    pub solo: Vec<u32>,
    pub volume_db: f64,
    pub mute: bool,
    /// The sound card it plays on, and its status.
    pub card: Option<String>,
    pub card_status: Option<Value>,
}

/// Closed captions.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Captions {
    pub on: bool,
    /// Seconds a caption stays.
    pub clear_after: f64,
    /// Recent lines.
    pub recent: Vec<Value>,
    /// Lines waiting.
    pub waiting: u64,
    /// Where they come from.
    pub from: Option<Value>,
}

/// A stinger slot's transition.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Stinger {
    /// The input played (a media file with alpha).
    pub input: u32,
    /// Seconds, and where in it the cut happens.
    pub duration: f64,
    pub cut: f64,
}

/// A desk's feed taken.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Feed {
    /// This switcher's output it feeds.
    pub output: Value,
    pub desk: String,
    /// The desk's channels (from 1).
    pub channels: [u32; 2],
    /// The desk heard from in the last 5 s.
    pub linked: bool,
}

/// A stream or a recording.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Stream {
    pub id: u64,
    pub target: String,
    /// Its name for people.
    pub name: String,
    /// `stream` or `recording`.
    pub kind: String,
    /// `connecting`, `live`, `retrying`, `failed` or `done`.
    pub state: String,
    pub error: String,
    /// The platform's answer typed (`auth_expired`, …).
    pub reason: Option<String>,
    /// The encoder not keeping up.
    pub overloaded: bool,
    pub bytes: u64,
    pub reconnects: u64,
    pub dropped: u64,
    pub skipped_pictures: u64,
    pub paused: bool,
    pub adaptive: bool,
    pub target_kbps: f64,
    pub video_kbps: f64,
    pub sent_kbps: f64,
    pub lag_ms: f64,
    /// `good`, `warning` or `bad`, and why.
    pub health: String,
    pub why: String,
    /// Since when it is live (Unix seconds).
    pub live_since: Option<u64>,
    /// The address it is sent from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// A recording's disk: bytes free and in all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_free: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_total: Option<u64>,
    /// The stream this one backs up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_of: Option<Value>,
    /// What this recording records (an ISO's input, a mix).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_of: Option<Value>,
    /// The sent rate, recent: `{at, kbps}`.
    #[serde(default)]
    pub sent_history: Vec<Value>,
}

/// Something wrong now.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Problem {
    /// Its name (`lost`, `format_mismatch`, `stream_failed`, …).
    pub problem: String,
    /// `info`, `warning` or `error`.
    pub level: String,
    /// For people.
    pub text: String,
    /// Its subject: `input`, `label`, `stream`, … as the problem has them.
    #[serde(flatten)]
    pub about: Map<String, Value>,
}

/// What undo and redo would do (null: nothing).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Undo {
    pub back: Option<String>,
    pub forward: Option<String>,
}

/// [`SwitcherState`]'s JSON schema.
pub fn state_schema() -> Value {
    schemars::schema_for!(SwitcherState).to_value()
}

/// The keys of `state` a [`SwitcherState`] read from it doesn't keep, as JSON pointers
/// (`/inputs/0/source/colour`); or why it couldn't be read.
pub fn undeclared(state: &Value) -> Result<Vec<String>, String> {
    let typed: SwitcherState = serde_json::from_value(state.clone()).map_err(|e| e.to_string())?;
    let back = serde_json::to_value(&typed).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    missing(state, &back, String::new(), &mut out);
    Ok(out)
}

fn missing(a: &Value, b: &Value, at: String, out: &mut Vec<String>) {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for (k, v) in x {
                let here = format!("{at}/{k}");
                match y.get(k) {
                    Some(w) => missing(v, w, here, out),
                    // (A null left out is the same.)
                    None if !v.is_null() => out.push(here),
                    None => {}
                }
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            for (i, (v, w)) in x.iter().zip(y).enumerate() {
                missing(v, w, format!("{at}/{i}"), out);
            }
        }
        _ => {}
    }
}
