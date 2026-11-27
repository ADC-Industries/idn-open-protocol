//! IDN in-ear monitoring (spec/extensions/idn-iem.md): the
//! desk marks channels "IEM" and sends them all, after their processing and before their
//! faders, as one flow; each musician's phone takes the flow and mixes it itself, by a mix
//! saved on the desk.
//!
//! This crate is what the desk and the phones share: the flow's wire format ([`wire`],
//! §I.4) and its coding ([`codec`]: one Opus stream per channel, mono, 48 kHz, 5 ms
//! frames). It depends on nothing of IDN's own, so the phone mixing library builds on it
//! for Android and iOS.
//!
//! The phone mixing library ([`receiver`], feature `receiver`, on by default) is the
//! receiving side: it takes the flow, mixes it by the musician's mix (followed live over
//! the desk's push channel) and sends push-to-talk back; [`ffi`] is its C ABI for Kotlin
//! (JNI) and Swift (`include/idn_iem.h`), [`play`] (feature `play`) plays it through the
//! system's sound card.

pub mod codec;
#[cfg(feature = "receiver")]
pub mod ffi;
#[cfg(feature = "play")]
pub mod play;
#[cfg(feature = "receiver")]
pub mod receiver;
pub mod wire;

/// The flow's sample rate (Opus's own): the desk converts to it.
pub const RATE: u32 = 48_000;

/// The desk's frames: 5 ms (240 samples at 48 kHz), for latency (§I.6).
pub const FRAME_SAMPLES: usize = 240;

/// The bit rate of each channel's stream, bit/s (mono; ~1.5 Mbit/s for 24 channels).
pub const BITRATE: u32 = 64_000;

/// The longest talkback frame a phone may send: 20 ms.
pub const MAX_TALK_SAMPLES: usize = 960;

/// How often a phone's session says it is still there (§I.5), and how long the desk
/// waits before it forgets one that went quiet.
pub const KEEPALIVE_MS: u16 = 1000;
pub const TIMEOUT_MS: u16 = 5000;
