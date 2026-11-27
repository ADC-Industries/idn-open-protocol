//! IDN virtual mixing desk (roadmap R.16, spec/extensions/idn-mixer.md): input channels
//! with trim, high-pass, gate, EQ, compressor, fader, pan and sends; a main bus, eight
//! aux buses and a monitor; meters and snapshots. Inputs come from the routing matrix and
//! the buses go back into it. An engine to embed (an embedding application builds its
//! interface on it).

pub mod card;
pub mod cues;
pub mod desk;
pub mod dsp;
pub mod engine;
pub mod fx;
pub mod iem;
pub mod netaudio;
pub mod osc;

pub mod plugins;
pub mod program;
pub mod record;
pub mod surface;
pub mod switchers;
pub mod x32;

pub use desk::{Desk, Settings};
pub use engine::typed::Command;
pub use engine::{Console, ConsoleConfig, saved_channel_count};
