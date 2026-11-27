//! The display service and its screens (idn-display.md §D.8): a screen's settings, and the
//! messages between the service and the `idn-display --commands` process it runs for each
//! screen, one JSON object a line on the process's standard input and output. The
//! process's standard input closing ends it (as `--parent-pipe`).

use crate::view::View;
use serde::{Deserialize, Serialize};

/// A screen's sound.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Audio {
    /// The sound card to play the feed's sound on (`""` or `"default"`: the system's); left
    /// out or null: no sound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// The feed's channels (from 0) on the card's first outputs, in order (at most 8).
    #[serde(default = "two")]
    pub channels: Vec<u16>,
    /// Its level, in dB (−60 to +12).
    #[serde(default)]
    pub volume_db: f64,
    #[serde(default)]
    pub mute: bool,
}

fn two() -> Vec<u16> {
    vec![0, 1]
}

impl Default for Audio {
    fn default() -> Self {
        Audio {
            device: None,
            channels: two(),
            volume_db: 0.0,
            mute: false,
        }
    }
}

impl Audio {
    /// The gain the samples are multiplied by.
    pub fn gain(&self) -> f32 {
        if self.mute {
            0.0
        } else {
            10f64.powf(self.volume_db.clamp(-60.0, 12.0) / 20.0) as f32
        }
    }

    pub fn check(&self) -> Result<(), String> {
        if self.channels.len() > 8 || self.channels.iter().any(|&c| c >= 64) {
            return Err("audio: channels, at most 8, each 0 to 63".into());
        }
        if !(-60.0..=12.0).contains(&self.volume_db) || self.volume_db.is_nan() {
            return Err("audio: volume_db −60 to +12".into());
        }
        if self
            .device
            .as_ref()
            .is_some_and(|d| d.chars().count() > 256)
        {
            return Err("audio: device name too long".into());
        }
        Ok(())
    }
}

/// What a screen shows once its feed has been gone `after_s`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Show {
    /// The last picture, held.
    Last,
    /// The logo (`logo`, else the site's: `logo.png` or `logo.jpg` in the machine's media
    /// folder, else the service's state folder; none: black).
    #[default]
    Logo,
    Black,
    /// The pictures of a folder in turn (`slideshow`, each `slide_s`).
    Slideshow,
}

/// What a screen does when its feed goes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Fallback {
    /// The last picture held this long (seconds, 0 to 3600) first.
    #[serde(default = "five")]
    pub after_s: f64,
    #[serde(default)]
    pub show: Show,
    /// A PNG or JPEG in the machine's media folder or the service's state folder; empty:
    /// the site's logo.
    #[serde(default)]
    pub logo: String,
    /// A folder of PNG and JPEG pictures, in the media folder or the state folder.
    #[serde(default)]
    pub slideshow: String,
    /// Each slide's time, in seconds (1 to 3600).
    #[serde(default = "eight")]
    pub slide_s: u32,
    /// Attempts to reach the feed again before giving up (the fallback then stays until
    /// the feed is set again); 0: never give up.
    #[serde(default)]
    pub reconnect_cap: u32,
}

fn five() -> f64 {
    5.0
}

fn eight() -> u32 {
    8
}

impl Default for Fallback {
    fn default() -> Self {
        Fallback {
            after_s: five(),
            show: Show::Logo,
            logo: String::new(),
            slideshow: String::new(),
            slide_s: eight(),
            reconnect_cap: 0,
        }
    }
}

impl Fallback {
    pub fn check(&self) -> Result<(), String> {
        if !(0.0..=3600.0).contains(&self.after_s) || self.after_s.is_nan() {
            return Err("fallback: after_s 0 to 3600".into());
        }
        if !(1..=3600).contains(&self.slide_s) {
            return Err("fallback: slide_s 1 to 3600".into());
        }
        if self.show == Show::Slideshow && self.slideshow.trim().is_empty() {
            return Err("fallback: a slideshow needs its folder (slideshow)".into());
        }
        Ok(())
    }
}

/// One screen's settings: where the picture goes, its sound, what it does when the feed
/// goes.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Settings {
    #[serde(flatten)]
    pub view: View,
    #[serde(default)]
    pub audio: Audio,
    #[serde(default)]
    pub fallback: Fallback,
    /// The screen's mode (size and refresh) while it shows a feed, one of its `modes`;
    /// none: as the desktop has it. Set by taking the screen whole (exclusive
    /// fullscreen): Windows, X11 and macOS; a Wayland desktop keeps its own mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<ScreenMode>,
    /// What the screen shows with no feed: the desktop (default), or its fallback (the
    /// logo or the slideshow), so a screen is never the desktop.
    #[serde(default)]
    pub idle: Idle,
}

/// What a screen with no feed shows.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Idle {
    #[default]
    Desktop,
    Fallback,
}

/// A screen mode: its size and refresh.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ScreenMode {
    pub width: u32,
    pub height: u32,
    /// Refresh, Hz (the nearest the screen offers is taken).
    #[serde(default)]
    pub refresh_hz: f64,
}

impl Settings {
    pub fn check(&self) -> Result<(), String> {
        crate::view::check(&self.view)?;
        self.audio.check()?;
        if let Some(m) = &self.mode
            && (!(64..=16384).contains(&m.width)
                || !(64..=16384).contains(&m.height)
                || !(0.0..=1000.0).contains(&m.refresh_hz))
        {
            return Err("mode: width and height 64 to 16384, refresh_hz 0 to 1000".into());
        }
        self.fallback.check()
    }
}

/// To a screen's process.
// (One message at a time, sent as a line of JSON: the settings' size costs nothing.)
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ToChild {
    /// Show this source (`HOST (name)`, a routing source, or `address:port`); empty:
    /// nothing (black). The picture shown stays until the new feed's first one.
    Feed { source: String },
    /// The settings, the fallback's files as full paths (checked by the service).
    Settings { settings: Settings },
    /// The screen's number and name, large, for `seconds`.
    Identify { lines: Vec<String>, seconds: f64 },
    /// What the screen shows, `width` pixels wide, as a JPEG.
    Thumbnail { id: u64, width: u32 },
    /// A test pattern on the whole screen (bars, a ramp, the edges and the centre) instead
    /// of what it shows, or off.
    TestPattern { on: bool },
}

/// The screen as the process found it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ScreenInfo {
    /// As the desktop names it (`DP-2`, `\\.\DISPLAY1`).
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// Its refresh rate in Hz (0: not known).
    pub refresh_hz: f64,
    /// Its place on the desktop.
    pub x: i32,
    pub y: i32,
    /// The modes it offers (largest first; empty: not known).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modes: Vec<ScreenMode>,
}

/// What a screen is doing.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// No feed set.
    #[default]
    Idle,
    /// Reaching a new feed (the picture before it held).
    Connecting,
    /// The feed's pictures on the screen.
    Playing,
    /// The feed gone, being reached again, the last picture held (for `after_s`).
    Lost,
    /// The fallback on the screen (`reason` says why).
    Fallback,
}

/// What is on the screen.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Showing {
    #[default]
    Black,
    /// The feed.
    Feed,
    /// The feed's last picture, held.
    Last,
    Logo,
    Slideshow,
    /// The test pattern (`test_pattern`).
    TestPattern,
}

/// The receiver's figures.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Stats {
    /// Received, over the last second or so, in kbit/s.
    pub bitrate_kbps: f64,
    /// Pictures received, and shown on the screen.
    pub received: u64,
    pub shown: u64,
    /// Pictures dropped (the screen didn't take them in time) and skipped as late
    /// (a newer one had come), since the feed was set.
    pub dropped: u64,
    pub late: u64,
    /// Pictures arriving a second, now.
    pub fps: f64,
    /// `gpu` (SpeedHQ reconstructed on the GPU, or a picture handed over on it) or `cpu`.
    pub decode: String,
    /// The feed's picture size, and the screen's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_size: Option<[u32; 2]>,
    pub screen_size: [u32; 2],
    /// The feed's frame rate as it says it, if it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_rate: Option<f64>,
    /// How the pictures come: `squic`, `quic`, `tcp`, `udp`, `multicast`, `machine`, ….
    pub transport: String,
    /// The session is encrypted (and by site trust).
    pub encrypted: bool,
    pub site_trust: bool,
    /// Times the feed was reached again after being lost.
    pub reconnects: u64,
    /// Pictures that failed to decode, since the feed was set.
    #[serde(default)]
    pub decode_errors: u64,
    /// Video-wall presentation in step (`sync/1`): its group, delay and late frames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<SyncStats>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SyncStats {
    pub group: String,
    pub mode: String,
    pub delay_ms: f64,
    pub late: u64,
}

/// A screen's status, as its process reports it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Status {
    pub state: State,
    pub showing: Showing,
    /// Why it lost the feed or shows the fallback.
    #[serde(default)]
    pub reason: String,
    /// The source a routing source sent this screen to (§1.8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routed_to: Option<String>,
    /// Attempts to reach the feed again since it was lost.
    #[serde(default)]
    pub attempts: u64,
    /// The reconnect cap was reached: no more attempts until the feed is set again.
    #[serde(default)]
    pub gave_up: bool,
    /// Seconds in this state.
    #[serde(default)]
    pub for_s: f64,
    #[serde(default)]
    pub stats: Stats,
}

/// From a screen's process.
// (One message at a time over a pipe, never stored in bulk: the size difference costs
// nothing worth a box.)
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum FromChild {
    /// The window is up on this screen, drawn by this GPU.
    Ready {
        screen: ScreenInfo,
        adapter: String,
    },
    Status {
        status: Status,
    },
    /// A thumbnail asked for: the JPEG in base64, or why not.
    Thumbnail {
        id: u64,
        #[serde(default)]
        jpeg: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_have_their_defaults_and_round_trip() {
        // The defaults: the last picture held, then the logo after 5 s; reconnecting
        // for ever.
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.fallback.after_s, 5.0);
        assert_eq!(s.fallback.show, Show::Logo);
        assert_eq!(s.fallback.reconnect_cap, 0);
        assert_eq!(s.audio.channels, [0, 1]);
        assert_eq!(s.audio.device, None);
        assert!(s.check().is_ok());
        let j = serde_json::json!({"rotation": 90, "overscan": 2.5, "fit": "fill",
            "audio": {"device": "", "channels": [2, 3], "volume_db": -6.0},
            "fallback": {"after_s": 2, "show": "black", "reconnect_cap": 10},
            "wall": {"rows": 2, "cols": 2, "row": 0, "col": 1,
                     "bezel_mm": {"h": 8, "v": 8}, "screen_mm": {"w": 1210, "h": 680}}});
        let s: Settings = serde_json::from_value(j).unwrap();
        assert_eq!(s.view.rotation, 90);
        assert_eq!(s.view.wall.as_ref().unwrap().col, 1);
        assert!(s.check().is_ok());
        let back: Settings = serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        assert!((s.audio.gain() - 0.501).abs() < 0.01);
        // Refused.
        let bad = |j: serde_json::Value| {
            serde_json::from_value::<Settings>(j).map_or(true, |s| s.check().is_err())
        };
        assert!(bad(serde_json::json!({"audio": {"volume_db": 20}})));
        assert!(bad(serde_json::json!({"audio": {"channels": [64]}})));
        assert!(bad(serde_json::json!({"fallback": {"after_s": -1}})));
        assert!(bad(serde_json::json!({"fallback": {"show": "slideshow"}})));
        assert!(bad(serde_json::json!({"fallback": {"show": "fireworks"}})));
        assert!(bad(serde_json::json!({"fit": "zoom"})));
    }

    #[test]
    fn messages_are_one_line_of_json_each() {
        let m = ToChild::Feed {
            source: "BOOTH (Program)".into(),
        };
        let line = serde_json::to_string(&m).unwrap();
        assert!(!line.contains('\n'));
        assert_eq!(
            line, r#"{"t":"feed","source":"BOOTH (Program)"}"#,
            "the process's protocol"
        );
        let back: ToChild = serde_json::from_str(&line).unwrap();
        assert_eq!(back, m);
        let st = FromChild::Status {
            status: Status {
                state: State::Lost,
                attempts: 3,
                ..Default::default()
            },
        };
        let back: FromChild = serde_json::from_str(&serde_json::to_string(&st).unwrap()).unwrap();
        assert_eq!(back, st);
    }
}
