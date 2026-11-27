//! SpeedHQ pictures reconstructed on the GPU from their coefficients (perf queue 2b): the
//! CPU reads the bitstream (`InputConfig::coefficients`), the GPU does the inverse DCT
//! and writes the samples. [`idct`] is the reconstruction itself (the switcher keeps the
//! pictures on the GPU for its compositor); [`Reconstructor`] reads them back, for a
//! receiver that hands samples on (the OBS plugin).

/// The inverse DCT itself, now in the library (`idn_open_protocol::shq::gpu_idct`).
pub use idn_open_protocol::shq::gpu_idct as idct;
mod readback;

pub use readback::{Gpu, Reconstructor};
