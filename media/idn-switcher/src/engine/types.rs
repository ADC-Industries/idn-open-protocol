//! The public types an input, a slideshow, the stream's sound, a timer, a trigger and a
//! stinger are set with.

use super::*;

/// What an input shows (§S.3).
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    /// An IDN or NDI-Compatible source: `HOST (name)` or an address.
    Network(String),
    /// A colour.
    Colour(Ycc),
    /// A picture (from a file), decoded once and shown as it is: not played as a looped
    /// one-frame file, which went through a video encode and decode a frame.
    Still { path: String, frame: Arc<Frame> },
    /// A live picture this process renders, pushed with [`Switcher::feed`] — a key, a lower
    /// third, a source your app composes — shown with no network round trip. Blank until
    /// the first `feed`.
    Feed,
    /// Another input shown `by_ms` milliseconds late (a delay input): the switcher keeps
    /// that input's recent pictures and replays them delayed, to line a camera up with a
    /// delayed feed or sound. Video only. `input` must already exist and not be this one.
    Delay { input: u32, by_ms: u32 },
    /// A scene (§S.12): layers placed, cropped, turned and blended over a transparent
    /// canvas — other inputs, other scenes, colours. Its layers are set with
    /// [`Switcher::scene_items`]; a new scene starts with a copy of every global layer
    /// (§S.5). Only scenes go on the buses. No sound of its own.
    Scene,
    /// A title (§S.16): text and boxes drawn over a transparent canvas — a name, a lower
    /// third, a caption. Its layers are set with [`Switcher::set_title`].
    Title,
    /// Pictures shown one after another (§S.16), by hand or every so often, cut or faded.
    /// Pictures are read with the loader given to [`Switcher::set_picture_loader`].
    Slideshow(Slideshow),
    /// A PTZ camera's preset as an input (§S.21): shows `camera` (an input), and moves the
    /// camera to `preset` when it goes to preview (unless the camera is on air through
    /// another of its inputs) or straight to program. The camera's control is set with
    /// [`Switcher::set_camera`].
    Ptz { camera: u32, preset: u8 },
    /// A shot cut from another input (§S.21, virtual PTZ): a part of its picture, moved and
    /// zoomed smoothly with [`Switcher::virtual_view`] — several shots from one wide camera.
    Virtual { input: u32, view: View },
}

/// Where a virtual shot looks (§S.21): the centre `x`, `y` as fractions of the source and
/// a zoom (1: the whole picture, 2: half its width; at most 8).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    pub x: f32,
    pub y: f32,
    pub zoom: f32,
}

impl View {
    pub const WHOLE: View = View {
        x: 0.5,
        y: 0.5,
        zoom: 1.0,
    };

    pub(super) fn clamped(self) -> View {
        View {
            x: if self.x.is_finite() {
                self.x.clamp(0.0, 1.0)
            } else {
                0.5
            },
            y: if self.y.is_finite() {
                self.y.clamp(0.0, 1.0)
            } else {
                0.5
            },
            zoom: if self.zoom.is_finite() {
                self.zoom.clamp(1.0, 8.0)
            } else {
                1.0
            },
        }
    }

    /// The view as shown: its centre moved to where the crop can be at its zoom (at zoom
    /// 2, x from 0.25 to 0.75 on a picture of the output's shape).
    pub(super) fn reachable(self, sw: usize, sh: usize, w: usize, h: usize) -> View {
        let (x0, y0, x1, y1) = self.crop(sw, sh, w, h);
        View {
            x: ((x0 + x1) / 2.0) as f32,
            y: ((y0 + y1) / 2.0) as f32,
            zoom: self.clamped().zoom,
        }
    }

    /// The part of a `sw` × `sh` picture it shows at a `w` × `h` output's shape: x0, y0,
    /// x1, y1 as fractions, kept inside the picture.
    pub fn crop(self, sw: usize, sh: usize, w: usize, h: usize) -> (f64, f64, f64, f64) {
        let v = self.clamped();
        let (sw, sh) = (sw.max(1) as f64, sh.max(1) as f64);
        let out = w.max(1) as f64 / h.max(1) as f64;
        // The biggest rectangle of the output's shape in the picture, then zoomed.
        let (mut cw, mut ch) = if sw / sh > out {
            (sh * out, sh)
        } else {
            (sw, sw / out)
        };
        cw /= v.zoom as f64;
        ch /= v.zoom as f64;
        let cx = (v.x as f64 * sw).clamp(cw / 2.0, sw - cw / 2.0);
        let cy = (v.y as f64 * sh).clamp(ch / 2.0, sh - ch / 2.0);
        (
            (cx - cw / 2.0) / sw,
            (cy - ch / 2.0) / sh,
            (cx + cw / 2.0) / sw,
            (cy + ch / 2.0) / sh,
        )
    }
}

/// A slideshow input's pictures and how it moves on.
#[derive(Debug, Clone, PartialEq)]
pub struct Slideshow {
    /// The pictures' files, in order.
    pub pictures: Vec<String>,
    /// Seconds each picture shows while playing (0: only by hand).
    pub every: f32,
    /// Seconds of cross-fade between pictures (0: a cut).
    pub fade: f32,
    /// Back to the first after the last (otherwise it stops there).
    pub looped: bool,
    /// In a random order (a new one each time round).
    pub shuffle: bool,
}

/// Moving a slideshow on ([`Switcher::slideshow`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlideAction {
    Next,
    Previous,
    /// A picture by its place in the list (from 0).
    Goto(usize),
    /// Moving on by itself every [`Slideshow::every`] seconds.
    Play,
    Pause,
}

/// Reads a picture file into a frame (any size; fitted to the switcher's). The program
/// gives one (FFmpeg's picture decoders); the engine reads no files itself.
pub type PictureLoader = Arc<dyn Fn(&str) -> Result<Frame, String> + Send + Sync>;

/// The stream's sound (§S.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioSource {
    None,
    /// An input's own sound.
    Input(u32),
    /// The program input's sound, following cuts.
    FollowProgram,
    /// What the routing matrix routes to the switcher's receiver (the desk's mix): needs
    /// [`SwitcherConfig::node`].
    Routed,
    /// A sound card's inputs `first` and `first + 1` (from 0; a mono card's one on both
    /// sides); `device` `None`: the default capture device (stage 3).
    Card {
        device: Option<String>,
        first: u16,
    },
}

/// An event a [`Trigger`] fires on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerEvent {
    /// This scene went to program (a cut or a finished transition put it on air).
    OnAir(u32),
    /// A transition finished (preview became program).
    TransitionEnd,
    /// This slideshow input, playing and not looping, reached its last picture.
    SlideshowEnd(u32),
    /// This timer counted down to zero, or reached its time of day (§S.19).
    TimerEnd(String),
}

/// How a timer counts (§S.19): shown in a title as `{timer:NAME}`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TimerMode {
    /// Down from this long to zero, while running.
    Down(Duration),
    /// Up from zero, while running.
    Up,
    /// Down to a time of day (the machine's local time), always: "the service starts in".
    Until { hour: u8, minute: u8 },
}

/// A change to a timer ([`Switcher::timer`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TimerAction {
    /// Makes it (or changes how it counts), stopped at its start.
    Set(TimerMode),
    Start,
    Stop,
    /// Back to its start, stopped.
    Reset,
    Remove,
}

/// What a [`Trigger`] does when its event fires — internal switcher actions only (no shell).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerAction {
    /// Cut a scene to program.
    Program(u32),
    /// Put a scene on preview.
    Preview(u32),
    /// Cut (swap program and preview).
    Cut,
    /// Fade to black on or off.
    Ftb { on: bool },
    /// Show or hide a scene's layer (by its id), with its fade (§S.12).
    LayerVisible { scene: u32, layer: u32, on: bool },
    /// Move a slideshow input on.
    Slide { input: u32, action: SlideAction },
}

/// An action run automatically when an event fires (vMix-style triggers). Set the list with
/// [`Switcher::set_triggers`]. Actions are internal only; a trigger whose action creates its
/// own event can chain, so avoid cycles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trigger {
    pub on: TriggerEvent,
    pub action: TriggerAction,
}

/// A stinger slot (§S.14): a clip input (usually a file with alpha) played over the bus as
/// the transition, `duration` seconds long; program and preview swap at `cut` seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stinger {
    pub input: u32,
    pub duration: f32,
    pub cut: f32,
}
