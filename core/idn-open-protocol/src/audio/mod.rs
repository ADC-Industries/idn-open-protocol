//! Sound processing without a sound card: lanes that play network sound against another
//! clock, summed by a [`Mixer`] (spec/extensions/idn-routing.md §A.7). The `idn-audio`
//! crate plays a mixer on a sound card (and captures sound cards); the OBS plugin pulls
//! one at OBS's pace; the video switcher and the mixing desk (roadmap R.15, R.16) build
//! on it too.

mod jitter;
mod mixer;
pub mod pace;
#[doc(hidden)]
pub mod resample;
#[doc(hidden)]
pub mod servo;

pub use mixer::{Lane, LaneConfig, LaneStats, Mixer, site_time};
