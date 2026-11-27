//! The desk's commands as typed values (an embedding program's way in, spec §M.5): each
//! command [`Console::command`] takes, as a [`Command`] run with [`Console::run`], plus
//! one-line helpers for the common live operations. Each serializes to exactly the JSON
//! the dispatcher reads (`{"cmd": NAME, "body": {...}}`). Its JSON schema: [`schema`]
//! (`GET /api/v1/schema`).
//!
//! The strip commands (`channel`, `bus`, `group`, `matrix`, `fx`) carry their settings as a
//! JSON merge patch: the settings are nested (EQ bands, dynamics, sends) and merged element
//! by element, so the patch stays JSON with its keys named in each variant's comment.

use super::Console;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

mod args;
mod methods;
pub use args::{
    AfvPatch, BusName, BusRef, IemSendPatch, InsertOp, NumText, Rate, RouteChange, ScopeChannels,
    SendBus, SendBusName, SnapshotScope, Strip, Transport, X32Patch,
};

/// One desk command (the body of `POST /api/v1/<cmd>`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "cmd", content = "body", rename_all = "snake_case")]
pub enum Command {
    /// Applies a built-in starter (names, colours, filters, groups); refused while locked.
    Starter {
        /// The starter's name, as the state's `starters` lists it.
        name: String,
    },
    /// Looks for plugins on this machine again (the list: `GET /api/v1/plugins`).
    PluginsScan {},
    /// Routes channels of a receiver through this machine's IDN Node (the desk's own routes only).
    Route {
        /// The receiver's name (the desk's own, or one the desk's transmitter feeds).
        receiver: String,
        /// The channels' routes, applied all or none.
        changes: Vec<RouteChange>,
    },
    /// Sets the latency of the desk's receiver or of one it feeds.
    ReceiverLatency {
        /// The receiver's name.
        receiver: String,
        /// Milliseconds, 1 to 500; absent or null: the receiving program's own.
        #[serde(skip_serializing_if = "Option::is_none")]
        ms: Option<u16>,
    },
    /// Locks or unlocks the desk (while locked, nothing changes but the lock).
    Lock {
        /// Locked; absent: true.
        #[serde(skip_serializing_if = "Option::is_none")]
        on: Option<bool>,
    },
    /// Changes a group bus by a merge patch (keys: name, color, eq_on, eq, comp, limiter, mute, fader (dB), pre, stereo, inserts, to_main, sources, source_names, listen, mono, dim, effect, geq, safe).
    Group {
        /// The group bus, from 1.
        group: usize,
        /// The bus settings merge patch (keys in the variant's comment; nested objects merge, null removes).
        #[serde(flatten)]
        patch: Map<String, Value>,
    },
    /// Changes a matrix mix by a merge patch (keys: name, color, eq_on, eq, comp, limiter, mute, fader (dB), pre, stereo, inserts, to_main, sources, source_names, listen, mono, dim, effect, geq, safe).
    Matrix {
        /// The matrix mix, from 1.
        matrix: usize,
        /// The bus settings merge patch (keys in the variant's comment; nested objects merge, null removes).
        #[serde(flatten)]
        patch: Map<String, Value>,
    },
    /// Changes an FX return bus by a merge patch (keys: name, color, eq_on, eq, comp, limiter, mute, fader (dB), pre, stereo, inserts, to_main, sources, source_names, listen, mono, dim, effect, geq, safe).
    Fx {
        /// The FX bus, from 1.
        fx: usize,
        /// The bus settings merge patch (keys in the variant's comment; nested objects merge, null removes).
        #[serde(flatten)]
        patch: Map<String, Value>,
    },
    /// Changes a mute group (only the keys given change).
    MuteGroup {
        /// The mute group, 1 to 8.
        group: usize,
        /// Its name.
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// On: its member channels muted.
        #[serde(skip_serializing_if = "Option::is_none")]
        on: Option<bool>,
        /// Its colour in panels (`#rrggbb`; empty: none).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<String>,
    },
    /// Changes talkback (only the keys given change).
    Talkback {
        /// The talkback microphone's channel, from 1; 0: none.
        #[serde(skip_serializing_if = "Option::is_none")]
        channel: Option<usize>,
        /// Talking.
        #[serde(skip_serializing_if = "Option::is_none")]
        on: Option<bool>,
        /// Its level, dB.
        #[serde(skip_serializing_if = "Option::is_none")]
        level: Option<f32>,
        /// The aux buses it goes to, one flag per bus (null: unchanged).
        #[serde(skip_serializing_if = "Option::is_none")]
        aux: Option<Vec<Option<bool>>>,
        /// It goes to the main bus.
        #[serde(skip_serializing_if = "Option::is_none")]
        main: Option<bool>,
        /// The main bus turned down this much while talkback is on, dB (−40 to 0; 0: not).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dim_main: Option<f32>,
        /// The talkback microphone in the IEM flow, heard there while talkback is on.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to_iem: Option<bool>,
    },
    /// Changes a user button (only the keys given change).
    Button {
        /// The button, 1 to 8.
        button: usize,
        /// Its label.
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        /// What it does: `mute:N`, `mute_group:N`, `dca_mute:N`, `snapshot:NAME`, `talkback`, `clear_solo`, `fx_mute` or empty.
        #[serde(skip_serializing_if = "Option::is_none")]
        action: Option<String>,
    },
    /// Presses a user button: does its action.
    Press {
        /// The button, 1 to 8.
        button: usize,
    },
    /// Clears every solo and listen.
    ClearSolo {},
    /// Resets the loudness meters.
    LoudnessReset {},
    /// Starts a multitrack recording.
    RecordStart {
        /// The channels recorded, from 1; absent: all.
        #[serde(skip_serializing_if = "Option::is_none")]
        channels: Option<Vec<usize>>,
        /// The recording's folder name; absent or empty: by date and time.
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Stops the multitrack recording.
    RecordStop {},
    /// Plays a recording back into the channels (virtual soundcheck), or stops that.
    Soundcheck {
        /// The recording's folder name; absent: stops the soundcheck.
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// The soundcheck playback's transport.
    SoundcheckTransport(Transport),
    /// Takes one channel live (its input) or back to the soundcheck playback.
    SoundcheckLive {
        /// The channel, from 1.
        n: usize,
        /// Live: from its input; absent: true.
        #[serde(skip_serializing_if = "Option::is_none")]
        live: Option<bool>,
    },
    /// Patches a desk output and/or sets its delay (at least one of them; what isn't given stays).
    Output {
        /// The desk output, from 1.
        out: usize,
        /// What it carries: `bus:J`, `input:N`, `preeq:N`, `posteq:N`, `pre:N`, `post:N`, `in:N` or empty.
        #[serde(skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        /// Its delay, ms (0 to 500).
        #[serde(skip_serializing_if = "Option::is_none")]
        delay: Option<f64>,
    },
    /// Changes desk-wide settings (only the keys given change).
    Desk {
        /// Solo is after the fader (AFL) rather than before it (PFL).
        #[serde(skip_serializing_if = "Option::is_none")]
        solo_afl: Option<bool>,
        /// Automatic parking: channels follow their routing on and off the desk.
        #[serde(skip_serializing_if = "Option::is_none")]
        auto_park: Option<bool>,
        /// The automix on.
        #[serde(skip_serializing_if = "Option::is_none")]
        automix: Option<bool>,
        /// The duckers, each a merge patch (name, on, keys, targets, threshold, depth, attack, hold, release; null: unchanged).
        #[serde(skip_serializing_if = "Option::is_none")]
        duckers: Option<Vec<Value>>,
        /// Audio follows video (only the keys given change).
        #[serde(skip_serializing_if = "Option::is_none")]
        afv: Option<AfvPatch>,
        /// Samples per block from the desk's next start: 32, 64, 128 or 256.
        #[serde(skip_serializing_if = "Option::is_none")]
        block: Option<usize>,
        /// The desk's sample rate from its next start: 44100, 48000 or 96000; `""` (`Rate::Text(String::new())`) for the machine's (null in JSON too). Absent (`None`): unchanged.
        #[serde(skip_serializing_if = "Option::is_none")]
        rate: Option<Rate>,
        /// Each desk output's delay, ms (null: unchanged).
        #[serde(skip_serializing_if = "Option::is_none")]
        output_delay: Option<Vec<Option<f32>>>,
        /// The Behringer X32 whose preamps the desk controls over Ethernet (§M.23.1); kept with the desk, not in snapshots.
        #[serde(skip_serializing_if = "Option::is_none")]
        x32: Option<X32Patch>,
    },
    /// Sets the X32 preamp behind input channel `n` (§M.23.1; at least one of the values). Not a channel setting: never saved, snapshotted or copied. Refused while the desk is locked, or when the channel's input is not an X32 USB channel carrying a preamp.
    Preamp {
        /// The channel, from 1.
        n: usize,
        /// The X32 preamp's gain, dB (-12 to 60, 0.5 dB steps).
        #[serde(skip_serializing_if = "Option::is_none")]
        gain_db: Option<f32>,
        /// 48 V phantom power on the X32 input.
        #[serde(skip_serializing_if = "Option::is_none")]
        phantom: Option<bool>,
        /// X32 polarity: inverts the X32's own channel strip, not what the desk receives over USB (the desk's `polarity` changes the desk's sound).
        #[serde(skip_serializing_if = "Option::is_none")]
        invert: Option<bool>,
    },
    /// Saves channel `n`'s settings as a preset.
    PresetSave {
        /// The preset's name.
        name: String,
        /// The channel, from 1.
        n: usize,
    },
    /// Loads a preset onto channel `n`.
    PresetLoad {
        /// The preset's name.
        name: String,
        /// The channel, from 1.
        n: usize,
    },
    /// Deletes a preset.
    PresetDelete {
        /// The preset's name.
        name: String,
    },
    /// The desk's channel count from its next start (1 to 64); `restart` to use it now.
    ChannelCount {
        /// Channels.
        n: usize,
    },
    /// The desk saved and started again at once by the IDN Node (refused when run by
    /// hand, or while it records).
    Restart {},
    /// The round trip measured: a click out of `output`, heard back on `input` (both from
    /// 1) looped back to it; answers `{ms, samples}`.
    LatencyTest {
        /// A desk output, from 1 (silent but for the click while it runs).
        output: usize,
        /// A desk input, from 1.
        input: usize,
    },
    /// A channel as on a fresh desk (processing, levels, sends, groups), its name,
    /// colour, inputs, failover, place on the desk and link kept.
    ChannelReset {
        /// The channel, from 1.
        n: usize,
    },
    /// Copies one channel's settings onto another.
    CopyChannel {
        /// The channel copied, from 1.
        from: usize,
        /// The channel copied onto, from 1.
        n: usize,
    },
    /// Changes an input channel by a merge patch (keys: name, color, source, input_b, use_b, failover, shown, trim, polarity, delay, hpf, lpf, hpf_on, lpf_on, hpf_last, lpf_last, hpf_slope, lpf_slope, gate, eq_on, eq, comp, deesser, limiter, denoise, denoise_amount, mute, fader (dB), pan, link, solo, sends, fx_sends, stream, inserts, insert_point, dca, main, groups, mute_groups, automix, automix_weight, follows, safe, iem). `hpf_on`/`lpf_on` switch a filter at its last frequency. A linked pair's second channel keeps its name, source, input_b, use_b and failover; the rest goes to the pair's first, whose settings it is given. Only an odd-numbered channel takes link: true.
    Channel {
        /// The channel, from 1.
        n: usize,
        /// The channel settings merge patch (keys in the variant's comment; nested objects merge, null removes).
        #[serde(flatten)]
        patch: Map<String, Value>,
    },
    /// Changes several input channels by the same merge patch, all or none (keys as `channel`).
    Channels {
        /// The channels, from 1.
        n: Vec<usize>,
        /// The channel settings merge patch (keys as `channel`'s).
        #[serde(flatten)]
        patch: Map<String, Value>,
    },
    /// Changes the main bus, an aux bus, the stream bus or the headphones by a merge patch (keys: name, color, eq_on, eq, comp, limiter, mute, fader (dB), pre, stereo, inserts, to_main, sources, source_names, listen, mono, dim, effect, geq, safe).
    Bus {
        /// The bus: "main", an aux bus from 1, "stream" or "headphones".
        bus: BusRef,
        /// The bus settings merge patch (keys in the variant's comment; nested objects merge, null removes).
        #[serde(flatten)]
        patch: Map<String, Value>,
    },
    /// Changes a DCA group (only the keys given change).
    Dca {
        /// The DCA group, 1 to 16.
        dca: usize,
        /// Its name.
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Its fader, dB (added to each member's).
        #[serde(skip_serializing_if = "Option::is_none")]
        fader: Option<f32>,
        /// Its members muted.
        #[serde(skip_serializing_if = "Option::is_none")]
        mute: Option<bool>,
        /// Its colour in panels (`#rrggbb`; empty: none).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<String>,
    },
    /// Saves the desk as a snapshot.
    SnapshotSave {
        /// The snapshot's name.
        name: String,
        /// It holds the IEM mixes and marks too (recalled with it); absent: false.
        #[serde(skip_serializing_if = "Option::is_none")]
        include_iem: Option<bool>,
    },
    /// Adds an IEM mix (answered with its id).
    IemMixAdd {
        /// The mix's name.
        name: String,
        /// Its id (letters, digits, '-', '_', '.'); absent: made from the name.
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    /// Changes an IEM mix (only the keys given change); an `iem:<id>` token's only command.
    IemMix {
        /// The mix's id.
        id: String,
        /// Its name.
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Its master level, dB (−144: off, to +10).
        #[serde(skip_serializing_if = "Option::is_none")]
        master_db: Option<f32>,
        /// The channels by id (the desk channel's number as text): each entry's keys given change; null takes the channel out of the mix.
        #[serde(skip_serializing_if = "Option::is_none")]
        channels: Option<std::collections::BTreeMap<String, Option<IemSendPatch>>>,
    },
    /// Deletes an IEM mix.
    IemMixDelete {
        /// The mix's id.
        id: String,
    },
    /// Adds a named output endpoint: some desk outputs as their own IDN output
    /// ("<desk> <name>"); answers its `id`.
    EndpointAdd {
        /// Its name (1 to 40 characters).
        name: String,
        /// The desk outputs it carries, from 1, in order.
        outputs: Vec<usize>,
    },
    /// Changes an endpoint's name and/or outputs (its IDN output is made again).
    Endpoint {
        /// The endpoint's id.
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        outputs: Option<Vec<usize>>,
    },
    /// Removes an endpoint.
    EndpointRemove {
        /// The endpoint's id.
        id: String,
    },
    /// Changes one channel's send into one aux bus or the stream bus (only the keys given
    /// change).
    Send {
        /// The channel, from 1.
        n: usize,
        /// The aux bus, from 1, or "stream".
        bus: SendBus,
        /// The send's level, dB (−144: off).
        #[serde(skip_serializing_if = "Option::is_none")]
        level: Option<f32>,
        /// On.
        #[serde(skip_serializing_if = "Option::is_none")]
        on: Option<bool>,
        /// Its place in a stereo aux bus, −1 to +1.
        #[serde(skip_serializing_if = "Option::is_none")]
        pan: Option<f32>,
        /// Before the channel's fader (true) or after.
        #[serde(skip_serializing_if = "Option::is_none")]
        pre: Option<bool>,
    },
    /// Recalls a snapshot (the recall-safe channels and buses stay).
    SnapshotRecall {
        /// The snapshot's name.
        name: String,
        /// What it takes; absent: everything.
        #[serde(skip_serializing_if = "Option::is_none")]
        scope: Option<SnapshotScope>,
        /// Faders, sends and pans moved over this many seconds (0 to 10; absent: 0.2).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fade_s: Option<f32>,
    },
    /// The snapshots' order: those named first, in this order; the rest after by name.
    SnapshotOrder {
        /// Snapshot names.
        names: Vec<String>,
    },
    /// Deletes a snapshot.
    SnapshotDelete {
        /// The snapshot's name.
        name: String,
    },
    /// Takes the site's switchers' outputs as desk inputs, or stops.
    SwitcherInputs {
        /// On; absent: true.
        #[serde(skip_serializing_if = "Option::is_none")]
        on: Option<bool>,
    },
    /// Takes one switcher's output as a desk input, or stops.
    SwitcherInput {
        /// The switcher's name.
        name: String,
        /// On; absent: true.
        #[serde(skip_serializing_if = "Option::is_none")]
        on: Option<bool>,
    },
    /// A switcher output's sound from a desk bus.
    SwitcherFeed {
        /// The switcher's name.
        name: String,
        /// The desk bus (as the state names it); absent or null: the switcher's own sound.
        #[serde(skip_serializing_if = "Option::is_none")]
        bus: Option<String>,
    },
    /// Changes a strip's plugin chain.
    Insert {
        /// The strip.
        #[serde(flatten)]
        strip: Strip,
        /// What changes.
        #[serde(flatten)]
        op: InsertOp,
    },
    /// Sets a plugin's parameter.
    PluginSet {
        /// The strip.
        #[serde(flatten)]
        strip: Strip,
        /// The plugin's place in the chain, from 0.
        index: usize,
        /// The parameter's id.
        param: u32,
        /// The value, in the parameter's range.
        value: f64,
    },
    /// Opens or closes a plugin's own window on the desk's machine.
    PluginWindow {
        /// The strip.
        #[serde(flatten)]
        strip: Strip,
        /// The plugin's place in the chain, from 0.
        index: usize,
        /// Open; absent: the other way round from now.
        #[serde(skip_serializing_if = "Option::is_none")]
        open: Option<bool>,
        /// The application's window to show it in (`x11:ID`, `hwnd:ID`); absent: its own.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
    },
}

/// The JSON schema of [`Command`].
/// `cmd` with `body` as its typed [`Command`], or why it isn't one (an unknown name, a
/// field of the wrong kind, one missing).
pub fn parse(cmd: &str, body: &Value) -> Result<Command, String> {
    let body = if body.is_null() {
        json!({})
    } else {
        body.clone()
    };
    serde_json::from_value(json!({"cmd": cmd, "body": body})).map_err(|e| {
        let e = e.to_string();
        if e.starts_with("unknown variant") {
            format!("no such command {cmd:?}")
        } else {
            format!("{cmd}: {e}")
        }
    })
}

pub fn schema() -> Value {
    serde_json::to_value(schemars::schema_for!(Command)).unwrap_or(Value::Null)
}

/// A one-key merge patch.
fn one(k: &str, v: Value) -> Map<String, Value> {
    Map::from_iter([(k.to_owned(), v)])
}

#[cfg(test)]
mod tests;
