//! A command's name and body, and the desk's typed helpers for the common live operations.

use super::*;

impl Command {
    /// The command's name (`POST /api/v1/<name>`).
    pub fn name(&self) -> &'static str {
        match self {
            Command::Starter { .. } => "starter",
            Command::PluginsScan {} => "plugins_scan",
            Command::Route { .. } => "route",
            Command::ReceiverLatency { .. } => "receiver_latency",
            Command::Lock { .. } => "lock",
            Command::Group { .. } => "group",
            Command::Matrix { .. } => "matrix",
            Command::Fx { .. } => "fx",
            Command::MuteGroup { .. } => "mute_group",
            Command::Talkback { .. } => "talkback",
            Command::Button { .. } => "button",
            Command::Press { .. } => "press",
            Command::ClearSolo {} => "clear_solo",
            Command::LoudnessReset {} => "loudness_reset",
            Command::RecordStart { .. } => "record_start",
            Command::RecordStop {} => "record_stop",
            Command::Soundcheck { .. } => "soundcheck",
            Command::SoundcheckTransport(_) => "soundcheck_transport",
            Command::SoundcheckLive { .. } => "soundcheck_live",
            Command::Output { .. } => "output",
            Command::Desk { .. } => "desk",
            Command::Preamp { .. } => "preamp",
            Command::PresetSave { .. } => "preset_save",
            Command::PresetLoad { .. } => "preset_load",
            Command::PresetDelete { .. } => "preset_delete",
            Command::ChannelCount { .. } => "channel_count",
            Command::Restart {} => "restart",
            Command::LatencyTest { .. } => "latency_test",
            Command::ChannelReset { .. } => "channel_reset",
            Command::CopyChannel { .. } => "copy_channel",
            Command::Channel { .. } => "channel",
            Command::Channels { .. } => "channels",
            Command::Bus { .. } => "bus",
            Command::Dca { .. } => "dca",
            Command::SnapshotSave { .. } => "snapshot_save",
            Command::SnapshotRecall { .. } => "snapshot_recall",
            Command::SnapshotOrder { .. } => "snapshot_order",
            Command::SnapshotDelete { .. } => "snapshot_delete",
            Command::IemMixAdd { .. } => "iem_mix_add",
            Command::IemMix { .. } => "iem_mix",
            Command::IemMixDelete { .. } => "iem_mix_delete",
            Command::Send { .. } => "send",
            Command::EndpointAdd { .. } => "endpoint_add",
            Command::Endpoint { .. } => "endpoint",
            Command::EndpointRemove { .. } => "endpoint_remove",
            Command::SwitcherInputs { .. } => "switcher_inputs",
            Command::SwitcherInput { .. } => "switcher_input",
            Command::SwitcherFeed { .. } => "switcher_feed",
            Command::Insert { .. } => "insert",
            Command::PluginSet { .. } => "plugin_set",
            Command::PluginWindow { .. } => "plugin_window",
        }
    }

    /// Its JSON body, as the dispatcher reads it (`{}` when none).
    pub fn body(&self) -> Value {
        serde_json::to_value(self)
            .ok()
            .map(|v| v["body"].clone())
            .filter(|b| !b.is_null())
            .unwrap_or_else(|| Value::Object(Map::new()))
    }
}

impl Console {
    /// Does a typed command (as [`Console::command`] with its name and body).
    pub fn run(&self, c: Command) -> Result<(), String> {
        self.command(c.name(), &c.body())
    }

    /// Channel `n`'s (from 1) fader, dB (−144 and below: off).
    pub fn set_fader(&self, n: usize, db: f32) -> Result<(), String> {
        self.run(Command::Channel {
            n,
            patch: one("fader", json!(db)),
        })
    }

    /// Mutes channel `n` (from 1) or not.
    pub fn set_mute(&self, n: usize, on: bool) -> Result<(), String> {
        self.run(Command::Channel {
            n,
            patch: one("mute", json!(on)),
        })
    }

    /// Renames channel `n` (from 1).
    pub fn rename_channel(&self, n: usize, name: &str) -> Result<(), String> {
        self.run(Command::Channel {
            n,
            patch: one("name", json!(name)),
        })
    }

    /// A bus's (main, aux, headphones) fader, dB.
    pub fn set_bus_fader(&self, bus: BusRef, db: f32) -> Result<(), String> {
        self.run(Command::Bus {
            bus,
            patch: one("fader", json!(db)),
        })
    }

    /// Mutes a bus (main, aux, headphones) or not.
    pub fn set_bus_mute(&self, bus: BusRef, on: bool) -> Result<(), String> {
        self.run(Command::Bus {
            bus,
            patch: one("mute", json!(on)),
        })
    }

    /// DCA group `k`'s (1–16) fader, dB.
    pub fn set_dca_fader(&self, k: usize, db: f32) -> Result<(), String> {
        self.run(Command::Dca {
            dca: k,
            name: None,
            fader: Some(db),
            color: None,
            mute: None,
        })
    }

    /// Mutes DCA group `k` (1–16) or not.
    pub fn set_dca_mute(&self, k: usize, on: bool) -> Result<(), String> {
        self.run(Command::Dca {
            dca: k,
            name: None,
            fader: None,
            color: None,
            mute: Some(on),
        })
    }

    /// Turns mute group `k` (1–8) on or off.
    pub fn set_mute_group(&self, k: usize, on: bool) -> Result<(), String> {
        self.run(Command::MuteGroup {
            group: k,
            name: None,
            color: None,
            on: Some(on),
        })
    }

    /// Talks on talkback or stops.
    pub fn set_talkback(&self, on: bool) -> Result<(), String> {
        self.run(Command::Talkback {
            channel: None,
            on: Some(on),
            level: None,
            aux: None,
            dim_main: None,
            to_iem: None,
            main: None,
        })
    }
}
