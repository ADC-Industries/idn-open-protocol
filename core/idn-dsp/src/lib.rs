//! Sound processing the IDN programs share, beside the protocol library: noise
//! suppression ([`denoise`]) for the mixer's channels and the switcher's stream sound
//! (it lived in the sound-card crate, `idn-audio`, which the switcher took for it alone).

pub mod denoise;
