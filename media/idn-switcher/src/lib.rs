//! IDN video switcher (roadmap R.15, spec/extensions/idn-switcher.md): inputs without a
//! fixed limit (IDN and NDI-Compatible sources, files, colours, titles…), received only
//! while in use, put into scenes (stacks of layers, global layers such as a logo in every
//! scene), scenes on program and preview with cut, mix, wipe, fade, stingers and luma
//! wipes, fade to black, the stream's sound, an adjustable multiview, and the program out
//! as an IDN or NDI-Compatible source. An engine to embed (an embedding application
//! builds its interface on it); nothing here draws one.

/// The start of a compositing error for one frame too large for the GPU's buffers (not the
/// GPU failing; bug run L4-4).
pub const TOO_LARGE: &str = "too large for the GPU: ";

#[cfg(feature = "card")]
pub mod card;
pub mod clock;
pub mod compose;
pub mod data;
pub mod displays;
pub mod engine;
pub mod filter;
pub mod font;
pub mod frame;
#[cfg(feature = "gpu")]
pub mod gpu;
#[cfg(feature = "gpu")]
pub mod gpu_filter;
#[cfg(feature = "gpu")]
pub(crate) mod gpu_hdr;
#[cfg(feature = "gpu")]
pub mod gpu_scene;
pub mod show;
/// The GPU reconstruction of SpeedHQ pictures (in `idn-shq-gpu`, shared with the OBS
/// plugin).
#[cfg(feature = "gpu")]
pub use idn_shq_gpu::idct;
pub mod jpeg;
pub mod multiview;
pub mod par;
pub mod ptz;
pub mod scene;
pub mod service;
pub mod streams;
pub mod title;

pub use compose::{Direction, Kind};
pub use engine::{
    AudioSource, InputTap, MAX_GLOBAL_LAYERS, MAX_SLIDES, MIXES, PictureLoader, STINGERS,
    SlideAction, Slideshow, Source, Stinger, Switcher, SwitcherConfig, TimerAction, TimerMode,
    TracksTap, Trigger, TriggerAction, TriggerEvent, View,
};
pub use frame::{Frame, Ycc};

/// GPU tests one at a time: devices made and used by several test threads at once crashed
/// the test process now and then (SIGSEGV, RADV, 1 run in 10–30).
#[cfg(test)]
pub(crate) static GPU_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());
