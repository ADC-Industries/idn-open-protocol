//! The switcher's commands as typed Rust values ([`Command`]), one variant per command
//! name that [`Service::command`] accepts, and their JSON schema ([`schema`], served as
//! `GET /api/v1/schema`). A [`Command`] is the same JSON underneath: [`Command::name`] and
//! [`Command::body`] are what `POST /api/v1/<name>` takes, and [`Service::run`] runs one
//! through [`Service::command`], so the JSON API and the typed one cannot drift apart.

use super::Service;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value, json};

mod args;
mod methods;
pub use args::{
    AudioSourceSpec, CameraControl, InputRef, ListAction, ListItem, ListSpec, MediaAction,
    MqttTriggerSpec, MultiviewTile, PtzMove, PtzShot, SlideMove, SlideStep, SlideshowSpec,
    TimeLength, TimerMode, TimerOp, View, VirtualShot,
};

/// A key that must be present but may be null.
fn nullable<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(d)
}

/// A key that may be missing (outer `None`), null (`Some(None)`) or a text.
fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}

/// One switcher command (§S.8): its name is the `cmd` tag, its JSON body the `body`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "cmd", content = "body", rename_all = "snake_case")]
pub enum Command {
    /// Sets the MQTT settings (§S.33) as a whole: broker, login, base topic and triggers.
    /// Not a partial update: a field left out is set empty (the password alone stays), so
    /// send every field, as read from the state's `mqtt`.
    MqttSet {
        /// The broker, `mqtt://host:port` (empty or left out: off).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        /// The login's user name (left out: none).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        username: Option<String>,
        /// The login's password (left out: the one there is stays).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password: Option<String>,
        /// The base topic (empty or left out: `idn/<machine>/switcher`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base: Option<String>,
        /// The triggers, as a whole (left out: none).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        triggers: Option<Vec<MqttTriggerSpec>>,
    },
    /// Adds a starter's inputs, timers and show (G6, §S.32).
    Starter {
        /// The starter's name.
        name: String,
    },
    /// Undoes the last edit of the setup (inputs, titles, scenes, filters, the show's
    /// cues). Not timers: the "countdown" timer a countdown title made stays.
    Undo {},
    /// Redoes the last undone edit.
    Redo {},
    /// Adds an input: give one of stream, list, file, network, colour, scene, title,
    /// slideshow, ptz or virtual.
    AddInput {
        /// The input's label (made from the source if missing).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        /// A stream's address as an input (SRT, RTMP, RTSP, HLS…).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stream: Option<String>,
        /// A list of videos and pictures as one input.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        list: Option<ListSpec>,
        /// A video, sound or picture file.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file: Option<String>,
        /// Whether a file plays again at its end.
        #[serde(default, rename = "loop", skip_serializing_if = "Option::is_none")]
        looped: Option<bool>,
        /// A network source's name or address.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        network: Option<String>,
        /// A plain colour, `#rrggbb`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        colour: Option<String>,
        /// True for a scene (items composed together).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scene: Option<bool>,
        /// A title: `{"layers": [...], "fields": {...}, "data": ...}` as `title` takes it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<Value>,
        /// A slideshow of pictures.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        slideshow: Option<SlideshowSpec>,
        /// A camera's preset as an input.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ptz: Option<PtzShot>,
        /// A part of another input's picture.
        #[serde(rename = "virtual", default, skip_serializing_if = "Option::is_none")]
        virtual_shot: Option<VirtualShot>,
        /// A title from a template by name: a saved one, or a style built in (`lower`,
        /// `two`, `caption`, `centre`, `corner`, `clock`, `count`, `score`; see `GET
        /// /api/v1/title_templates`). A countdown title makes a 5-minute "countdown" timer
        /// if there is none (an undo of the title leaves the timer). A saved template takes
        /// `text` in its one field (or its one plain text layer), else as the label only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        template: Option<String>,
        /// The text a built-in style shows, or a saved template with one place for text
        /// (and the label, cut at 24 characters, unless `label` is given); `clock`,
        /// `count` and `score` need none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    /// Moves a list input on (§S.18).
    List {
        /// The list input.
        id: InputRef,
        /// What to do.
        action: ListAction,
        /// The item for `goto`, from 0.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        n: Option<u64>,
    },
    /// Replaces a list input's items.
    ListItems {
        /// The list input.
        id: InputRef,
        /// The items, in order.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        items: Option<Vec<ListItem>>,
        /// A folder whose media files are added, by name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        folder: Option<String>,
        /// Files separated by `|`, or a folder (one name with no extension).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        /// Seconds each picture shows (default 5).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seconds: Option<f64>,
    },
    /// Plays, pauses or seeks a media file input.
    Media {
        /// The media input.
        id: InputRef,
        /// What to do.
        action: MediaAction,
        /// Seconds from the start, for `seek`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        position: Option<f64>,
    },
    /// Sets an input's filters, as a whole, in order (§S.17).
    Filters {
        /// The input.
        id: InputRef,
        /// The filters: a list of filter objects as the state shows them.
        filters: Value,
    },
    /// Sets a title's layers, as a whole (§S.16).
    Title {
        /// The title input.
        id: InputRef,
        /// The layers: a list of layer objects (text, picture, shape…) as the state shows them.
        layers: Vec<Value>,
        /// The title's fields, `{"name": value}` (text, numbers or true/false).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fields: Option<Map<String, Value>>,
        /// The title's data source (a file or address read into rows), or none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
    },
    /// Sets some of a title's fields, live (§S.19).
    TitleFields {
        /// The title input.
        id: InputRef,
        /// The fields to change, `{"name": value}` (text, numbers or true/false).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fields: Option<Map<String, Value>>,
    },
    /// Sets how a camera is controlled (§S.21).
    Camera {
        /// The camera input.
        id: InputRef,
        /// The control; missing or null for none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        control: Option<CameraControl>,
    },
    /// Moves a camera.
    Ptz {
        /// The camera, or one of its preset inputs.
        id: InputRef,
        /// The move.
        #[serde(rename = "move")]
        movement: PtzMove,
        /// Pan speed for `drive`, −1 to 1 (right is positive).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pan: Option<f64>,
        /// Tilt speed for `drive`, −1 to 1 (up is positive).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tilt: Option<f64>,
        /// Zoom speed for `zoom` (−1 out … 1 in), or the speed of `recall` (0–1).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        speed: Option<f64>,
        /// The preset for `recall` and `store`, 1 to 16.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preset: Option<u64>,
    },
    /// Moves a virtual shot.
    VirtualView {
        /// The virtual shot input.
        id: InputRef,
        /// Where it goes (the whole picture if missing).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        view: Option<View>,
        /// Seconds the move takes (0: at once).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seconds: Option<f64>,
    },
    /// Moves a title's data on a row, or reads it again (§S.20).
    TitleRow {
        /// The title input.
        id: InputRef,
        /// What to do (`next`, `previous`, `play`, `pause`, `goto` or `read`).
        action: SlideMove,
        /// The row for `goto`, from 0.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        n: Option<u64>,
    },
    /// Adds a number to a title's field (a score); answers its new value.
    TitleFieldAdd {
        /// The title input.
        id: InputRef,
        /// The field's name.
        name: String,
        /// What is added (default 1).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<f64>,
    },
    /// Keeps a title as a template, or removes one.
    TitleTemplate {
        /// The template's name, 1–64 characters.
        name: String,
        /// The title (`{"layers", "fields"}`); missing or null removes the template.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<Value>,
    },
    /// Sets, starts, stops, resets or removes a timer (§S.19).
    Timer {
        /// The timer's name.
        name: String,
        /// What to do.
        action: TimerOp,
        /// How it counts, for `set` (default down).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<TimerMode>,
        /// How long, for `down`: seconds, or `"m:ss"`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seconds: Option<TimeLength>,
        /// The time of day, `HH:MM`, for `until`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        until: Option<String>,
    },
    /// Moves a slideshow on.
    Slideshow {
        /// The slideshow input.
        id: InputRef,
        /// What to do (`next`, `previous`, `play`, `pause` or `goto`).
        action: SlideMove,
        /// The picture for `goto`, from 0.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        n: Option<u64>,
    },
    /// Moves the slideshow or list on program on, else the one on preview, else the
    /// first; answers its `id` and `kind` (`slideshow` or `list`).
    Slide {
        /// What to do.
        action: SlideStep,
        /// The picture or item for `goto`, from 0.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        n: Option<u64>,
    },
    /// Sets the sound of one layer of a scene: by `layer` (its id) or `item` (its place).
    SceneItemSound {
        /// The scene input.
        id: InputRef,
        /// The layer's place, from 0 (without `layer`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item: Option<u64>,
        /// The layer's id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        layer: Option<u32>,
        /// Its volume in dB (unchanged if missing).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        volume_db: Option<f64>,
        /// Whether it is muted (unchanged if missing).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        muted: Option<bool>,
    },
    /// Sets a scene's layers, as a whole, bottom to top, at most 16 (§S.12). A layer with
    /// an `id` the scene has keeps it; one without gets a new one. A layer with `global`
    /// is that global layer's copy (only its place and `visible` are its own).
    SceneItems {
        /// The scene input.
        id: InputRef,
        /// The layers: a list of layer objects as the state shows them (`id`, `name`,
        /// `volunteer`, `fade`, `global`, `source`, `rect`, `crop`, `rotation`, `opacity`,
        /// `blend`, `border`, `border_colour`, `radius`, `key`, `visible`, `volume_db`,
        /// `muted`).
        items: Value,
    },
    /// Shows or hides one layer of a scene, fading over the layer's `fade` seconds; live
    /// (not an undo step).
    SceneLayerVisible {
        /// The scene.
        scene: InputRef,
        /// The layer's id.
        layer: u32,
        /// Shown (default true) or hidden.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on: Option<bool>,
        /// Seconds this once (0: a cut); none: the layer's own fade.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fade: Option<f32>,
    },
    /// Defines a global layer, or changes one (§S.5): answers its id. A new one is added to
    /// every scene made from then on; a change reaches every scene's copy (each keeps its
    /// place and visibility).
    GlobalLayer {
        /// The layer, as a scene's layer object; with the `id` of a global layer there is,
        /// that one is changed. Its `visible`: whether new scenes' copies show it.
        layer: Value,
        /// A new one is also added, on top, to every scene there is (default false).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        add_to_scenes: Option<bool>,
    },
    /// Removes a global layer, and its copy from every scene.
    GlobalLayerRemove {
        /// The global layer's id.
        id: u32,
    },
    /// Keeps an input receiving while it isn't in use (§S.3), or lets it disconnect then.
    KeepActive {
        /// The input.
        id: InputRef,
        /// Kept active (default true) or not.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on: Option<bool>,
    },
    /// The program format (`1080p30`, `1080p60`, `2160p30`, `2160p60`, `WxH@FPS`), not
    /// while anything streams or records: kept, and under the IDN Node the switcher starts
    /// again with it at once; run by hand, from its next start (the state's
    /// `format_next`).
    Format { format: String },
    /// Saves a stream destination (a new one, or `id`'s): its name and the `stream_add`
    /// body that starts it (its key kept by the switcher, never shown); answers its id.
    DestinationSave {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        name: String,
        body: Value,
    },
    /// Removes a saved destination.
    DestinationRemove { id: String },
    /// Starts saved destinations (as `stream_add` would); answers `{started, errors}`.
    GoLive { ids: Vec<String> },
    /// A scene's desk snapshot recalled when it goes to program (desk and snapshot both
    /// given; either left out or empty: none).
    SceneSnapshot {
        /// The scene.
        id: InputRef,
        /// The desk's source name ("HOST (Mixer)").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        desk: Option<String>,
        /// The snapshot's name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        snapshot: Option<String>,
    },
    /// Sets the multiview's layout (§S.7), or the default.
    MultiviewLayout {
        /// The tiles, in drawing order (at most 128); null or missing: the default layout.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tiles: Option<Vec<MultiviewTile>>,
    },
    /// Sends the multiview on the network, or stops it (§S.7).
    Multiview {
        /// Sent or not.
        output: bool,
    },
    /// Removes an input.
    RemoveInput {
        /// The input.
        id: InputRef,
    },
    /// Renames an input.
    RenameInput {
        /// The input.
        id: InputRef,
        /// Its new label.
        label: String,
    },
    /// Delays an input's sound for lip sync.
    AudioOffset {
        /// The input.
        id: InputRef,
        /// The delay in milliseconds, 0–500.
        ms: u64,
    },
    /// Puts a scene on preview (only scenes go on the buses).
    Preview {
        /// The scene.
        id: InputRef,
    },
    /// Puts a scene on program at once.
    Program {
        /// The scene.
        id: InputRef,
    },
    /// Swaps preview and program at once.
    Cut {},
    /// Swaps preview and program with the transition.
    Auto {},
    /// Moves the T-bar.
    Tbar {
        /// The T-bar's position, 0–1.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        position: Option<f64>,
    },
    /// Sets the program's transition.
    Transition {
        /// cut, mix, fade, wipe-left|right|up|down|circle, zoom, luma or stinger-1…8 (default
        /// mix).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        /// Its length in seconds (default 1).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration: Option<f64>,
        /// The softness of a wipe's or luma's edge, 0–1 (default 0.05).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        soft: Option<f64>,
        /// The luma pattern's input id (for `luma`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<u32>,
        /// Whether the luma pattern is inverted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        invert: Option<bool>,
    },
    /// Sets or empties a stinger slot (§S.14).
    Stinger {
        /// The slot, 1–8.
        n: u64,
        /// The stinger's input id; missing or null empties the slot.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<u32>,
        /// Its length in seconds (default 1).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration: Option<f64>,
        /// When the cut under it happens, in seconds (default 0.5).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cut: Option<f64>,
    },
    /// Fades the program to black, or back.
    Ftb {},
    /// Sets the program's sound: its source, trim, delay, noise reduction and feed (§S.26).
    Audio {
        /// The source (without it, only `take_feed` and `denoise` may be given).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<AudioSourceSpec>,
        /// The trim in dB, with `source` (unchanged if missing).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gain_db: Option<f64>,
        /// The lip-sync delay in milliseconds, with `source` (unchanged if missing).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delay_ms: Option<u64>,
        /// Whether the sound is taken from the desk's feed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        take_feed: Option<bool>,
        /// Whether noise reduction is on.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        denoise: Option<bool>,
    },
    /// Sets the monitor (§S.24); each part optional.
    Monitor {
        /// Whether it hears preview instead of program.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview: Option<bool>,
        /// The inputs soloed (their ids), as a whole.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        solo: Option<Vec<u32>>,
        /// Its volume in dB.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        volume_db: Option<f64>,
        /// Whether it is muted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mute: Option<bool>,
        /// The sound card it plays on (`""` the default), null for none; missing: unchanged.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "present"
        )]
        #[schemars(with = "Option<String>")]
        card: Option<Option<String>>,
    },
    /// Solos one input on the monitor, or not.
    MonitorSolo {
        /// The input's id.
        input: u32,
        /// Soloed or not (default true).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on: Option<bool>,
    },
    /// Shows an output on one of this machine's screens, or turns it off (idn-display §D.3).
    Screen {
        /// The screen: its number or name (`"1"`, `"kms:HDMI-A-1"`).
        screen: String,
        /// The output's full name; missing or `""` turns the screen off.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        /// The sound card it plays on, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio: Option<String>,
    },
    /// Puts a line of closed captions on the program (§S.29).
    Caption {
        /// The line.
        text: String,
    },
    /// Sets the closed captions: on or off, when they clear, where they come from.
    Captions {
        /// On or off (unchanged if missing).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on: Option<bool>,
        /// Seconds after which a line clears (unchanged if missing).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        clear_after: Option<f64>,
        /// A captions source (G2), or `""` for none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        /// A global layer (its id) shown or hidden in every scene with `on`: the visible
        /// captions with the closed captions (one CC button).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        layer: Option<u32>,
    },
    /// Saves the setup under a name (§S.28).
    SetupSave {
        /// The setup's name.
        name: String,
    },
    /// Loads a saved setup.
    SetupLoad {
        /// The setup's name.
        name: String,
    },
    /// Deletes a saved setup.
    SetupDelete {
        /// The setup's name.
        name: String,
    },
    /// Fires the next cue of the show (§S.27); answers the cue's errors.
    CueGo {
        /// Only if this cue (from 0) is still the next (the follow-on's check).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        if_next: Option<u64>,
    },
    /// Fires the cue before the current one again.
    CueBack {},
    /// Makes a cue the next one.
    CueStandby {
        /// The cue, from 1.
        n: u64,
    },
    /// Fires a cue now; answers its errors.
    CueFire {
        /// The cue, from 1.
        n: u64,
    },
    /// Replaces the show, as a whole; answers its new `version`.
    ShowSet {
        /// The show: `{"name", "cues": [...]}` as the state shows it.
        show: Value,
        /// Only if the show is still at this version (`show.version` in the state).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        if_version: Option<u64>,
    },
    /// Adds a cue; answers its place `n` and the show's `version`.
    CueAdd {
        /// The cue (`{"name", "notes", "kind", "length", "follow", "actions"}`); missing:
        /// an empty one named "Cue N".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cue: Option<Value>,
        /// Its place, from 1 (the end if missing).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<u64>,
        /// Only if the show is still at this version.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        if_version: Option<u64>,
    },
    /// Changes a cue: the keys given, the others as they are.
    CueUpdate {
        /// The cue, from 1.
        n: u64,
        /// The keys to change.
        cue: Value,
        /// Only if the show is still at this version.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        if_version: Option<u64>,
    },
    /// Deletes a cue.
    CueDelete {
        /// The cue, from 1.
        n: u64,
        /// Only if the show is still at this version.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        if_version: Option<u64>,
    },
    /// Moves a cue to another place.
    CueMove {
        /// The cue, from 1.
        n: u64,
        /// Its new place, from 1.
        to: u64,
        /// Only if the show is still at this version.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        if_version: Option<u64>,
    },
    /// Starts a new, empty show.
    ShowNew {
        /// The new show's name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Loads a saved show.
    ShowLoad {
        /// The saved show's name.
        name: String,
    },
    /// Saves the show under a name.
    ShowSave {
        /// The name it is saved as.
        name: String,
    },
    /// Deletes a saved show.
    ShowDelete {
        /// The saved show's name.
        name: String,
    },
    /// Logs in to Planning Center with a personal access token (§S.27).
    PcoLogin {
        /// The token's application id.
        app_id: String,
        /// The token's secret.
        secret: String,
    },
    /// Forgets the Planning Center token.
    PcoLogout {},
    /// Answers the Planning Center plans.
    PcoPlans {},
    /// Imports a Planning Center plan as the show.
    PcoImport {
        /// The plan's service type id.
        service_type: String,
        /// The plan's id.
        plan: String,
    },
    /// Starts a stream or a recording of the program or a mix (§S.30); answers its id.
    StreamAdd {
        /// The stream's address: rtmp://, rtmps:// or srt://.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        /// A recording's file (.mp4 or .mkv).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file: Option<String>,
        /// Inputs whose own sound goes in extra tracks of a recording (their ids).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tracks: Option<Vec<u32>>,
        /// This machine's address the stream is sent from (RTMP).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        via: Option<String>,
        /// A backup ingest's address beside the stream.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        backup: Option<String>,
        /// The address the backup is sent from.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        backup_via: Option<String>,
        /// A file the stream's encode is also recorded to.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        record: Option<String>,
        /// The stream's name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// A mix's id to send instead of the program.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mix: Option<u64>,
        /// The picture size, `WxH` (as 1280x720).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<String>,
        /// The video codec: h264, hevc or av1.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codec: Option<String>,
        /// The video rate in Mbit/s (0.1–200).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        video_mbps: Option<f64>,
        /// A recording split into parts every so many seconds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        split_s: Option<u64>,
        /// A recording split into parts every so many megabytes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        split_mb: Option<u64>,
        /// Whether a stream's rate follows the upload (default false).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        adaptive: Option<bool>,
        /// The lowest rate an adaptive stream goes to, in Mbit/s.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        floor_mbps: Option<f64>,
    },
    /// Records one input on its own (ISO); answers its id and the file written (its full
    /// path: " (2)" etc. after a name already taken).
    IsoAdd {
        /// The input's id.
        input: u32,
        /// The file, .mp4 or .mkv (default: the input's label, .mp4).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file: Option<String>,
        /// Split into parts every so many seconds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        split_s: Option<u64>,
        /// Split into parts every so many megabytes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        split_mb: Option<u64>,
    },
    /// Adds an independent mix (§S.15); answers its id.
    AddMix {
        /// Its size: `WxH`, `1080p`, `2160p`… (or give width and height).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
        /// Its width in pixels, without `format`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width: Option<u64>,
        /// Its height in pixels, without `format`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        height: Option<u64>,
        /// Its name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// It follows the program (its scenes and transitions), the program's picture
        /// centre-cropped to its shape: a vertical version of the program.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        follow: Option<bool>,
    },
    /// Removes a mix.
    RemoveMix {
        /// The mix's id.
        mix: u32,
    },
    /// Puts a scene on a mix's program at once.
    MixProgram {
        /// The mix's id.
        mix: u32,
        /// The scene.
        id: InputRef,
    },
    /// Puts a scene on a mix's preview.
    MixPreview {
        /// The mix's id.
        mix: u32,
        /// The scene.
        id: InputRef,
    },
    /// Swaps a mix's preview and program at once.
    MixCut {
        /// The mix's id.
        mix: u32,
    },
    /// Swaps a mix's preview and program with its transition.
    MixAuto {
        /// The mix's id.
        mix: u32,
    },
    /// Sets a mix's transition.
    MixTransition {
        /// The mix's id.
        mix: u32,
        /// cut, mix, fade, wipe-…, luma (default mix).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        /// Its length in seconds (default 1).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration: Option<f64>,
        /// The softness of a wipe's or luma's edge, 0–1 (default 0.05).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        soft: Option<f64>,
        /// The luma pattern's input id (for `luma`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<u32>,
        /// Whether the luma pattern is inverted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        invert: Option<bool>,
    },
    /// Pauses or resumes a stream or recording.
    StreamPause {
        /// The stream's id.
        id: u64,
        /// Paused or not (default true).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        paused: Option<bool>,
    },
    /// Stops and removes a stream or recording.
    StreamRemove {
        /// The stream's id.
        id: u64,
    },
}

/// The JSON schema of [`Command`]: every command, its body's keys and their meaning.
pub fn schema() -> Value {
    schemars::schema_for!(Command).to_value()
}

#[cfg(test)]
mod tests;
