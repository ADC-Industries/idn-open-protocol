//! The display service's commands as typed values ([`Command`]), one variant per command
//! name `POST /api/v1/<name>` takes, and their JSON schema (`GET /api/v1/schema`), as the
//! switcher's and the desk's. [`super::Service::command`] parses the JSON into one, so the
//! JSON API and the typed one cannot drift apart.

use crate::proto::{Audio, Fallback, Idle, ScreenMode};
use crate::view::{Fit, Wall};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// A screen: its number (from 1, in the state's order of the screens there now) or its
/// name (`DP-2`, `\\.\DISPLAY1`, as the state's `id`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum ScreenRef {
    Number(u64),
    Name(String),
}

impl From<&str> for ScreenRef {
    fn from(s: &str) -> Self {
        ScreenRef::Name(s.to_owned())
    }
}

impl From<u64> for ScreenRef {
    fn from(n: u64) -> Self {
        ScreenRef::Number(n)
    }
}

/// A key that may be missing (outer `None`), null (`Some(None)`) or a value.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

/// One display service command (§D.8.3): its name is the `cmd` tag, its JSON body the
/// `body`.
// (One command at a time, parsed and applied: the size difference costs nothing worth a
// box, and boxing would show in the JSON schema's Rust users.)
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "cmd", content = "body", rename_all = "snake_case")]
pub enum Command {
    /// What a screen shows from now on: a source by its name (`HOST (name)`), a routing
    /// source to follow wherever it is switched (§1.8), or `address:port`; empty: nothing
    /// (the screen goes back to the desktop). The picture on the screen stays until the
    /// new feed's first one: no black between.
    Feed { screen: ScreenRef, source: String },
    /// Changes a screen's settings: the fields given; `audio`, `fallback` and `wall` each
    /// as a whole (send every field, as read from the state's `settings`); `wall: null`
    /// takes the screen out of a wall.
    Settings {
        screen: ScreenRef,
        /// 0, 90, 180 or 270 (clockwise).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rotation: Option<u16>,
        /// The part of each edge the screen hides, in % (0 to 20).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        overscan: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fit: Option<Fit>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio: Option<Audio>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fallback: Option<Fallback>,
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        #[schemars(with = "Option<Wall>")]
        wall: Option<Option<Wall>>,
        /// The screen's mode, one of its `modes`; null: as the desktop has it.
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        #[schemars(with = "Option<ScreenMode>")]
        mode: Option<Option<ScreenMode>>,
        /// What it shows with no feed: `desktop` or `fallback`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        idle: Option<Idle>,
    },
    /// The screen's number and name (and feed), large over its picture, for `seconds`
    /// (default 5, 0.5 to 600): one screen, or `"all"`. A screen with no feed shows it on
    /// black, then goes back to the desktop.
    Identify {
        screen: ScreenRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seconds: Option<f64>,
    },
    /// Forgets a screen's saved feed and settings (a screen no longer there).
    Forget { screen: ScreenRef },
    /// A screen's name for people (`Stage left`, at most 60 characters); empty: none.
    Label { screen: ScreenRef, label: String },
    /// A test pattern on a screen (or `"all"`), for `seconds` (1 to 3600) or until turned
    /// off; the screen's own picture is back after it.
    TestPattern {
        screen: ScreenRef,
        on: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seconds: Option<f64>,
    },
    /// The screen's process started again (its feed and settings kept).
    Restart { screen: ScreenRef },
    /// The API on the network on or off (kept); on this machine it stays open.
    Lan { on: bool },
}

#[cfg(test)]
impl Command {
    /// Its name, as `POST /api/v1/<name>` takes it.
    pub fn name(&self) -> &'static str {
        match self {
            Command::Feed { .. } => "feed",
            Command::Settings { .. } => "settings",
            Command::Identify { .. } => "identify",
            Command::Forget { .. } => "forget",
            Command::Label { .. } => "label",
            Command::TestPattern { .. } => "test_pattern",
            Command::Restart { .. } => "restart",
            Command::Lan { .. } => "lan",
        }
    }

    /// Its JSON body.
    pub fn body(&self) -> Value {
        serde_json::to_value(self)
            .ok()
            .and_then(|v| v.get("body").cloned())
            .unwrap_or(Value::Null)
    }
}

/// The commands and their bodies, as a JSON schema.
pub fn schema() -> Value {
    schemars::schema_for!(Command).to_value()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn commands_parse_from_the_api_json_and_back() {
        let c: Command = serde_json::from_value(
            json!({"cmd": "feed", "body": {"screen": 2, "source": "BOOTH (Program)"}}),
        )
        .unwrap();
        assert_eq!(
            c,
            Command::Feed {
                screen: ScreenRef::Number(2),
                source: "BOOTH (Program)".into()
            }
        );
        assert_eq!(c.name(), "feed");
        assert_eq!(c.body()["source"], "BOOTH (Program)");
        // The wall: missing (unchanged), null (out of the wall) or one.
        let s = |body| serde_json::from_value::<Command>(json!({"cmd": "settings", "body": body}));
        match s(json!({"screen": "DP-2", "rotation": 90})).unwrap() {
            Command::Settings { wall, rotation, .. } => {
                assert_eq!((wall, rotation), (None, Some(90)))
            }
            _ => panic!(),
        }
        match s(json!({"screen": "DP-2", "wall": null})).unwrap() {
            Command::Settings { wall, .. } => assert_eq!(wall, Some(None)),
            _ => panic!(),
        }
        match s(json!({"screen": "DP-2", "wall": {"rows": 2, "cols": 2, "row": 0, "col": 1}}))
            .unwrap()
        {
            Command::Settings { wall, .. } => assert_eq!(wall.unwrap().unwrap().col, 1),
            _ => panic!(),
        }
        assert!(s(json!({"screen": "DP-2", "fit": "zoom"})).is_err());
        let sc = schema().to_string();
        for name in [
            "feed",
            "settings",
            "identify",
            "forget",
            "reconnect_cap",
            "bezel_mm",
        ] {
            assert!(sc.contains(name), "{name} in the schema");
        }
    }
}
