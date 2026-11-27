//! IDN Voice's client library (spec/extensions/idn-voice.md §V.13, §V.14; the protocol in
//! docs/VOICE-CLIENT.md): what a handset app links to be a Voice client without writing
//! the protocol again. Built for a fleet app on Android, on the Inrico B01 (trucks that
//! change networks all day), through its C ABI ([`ffi`], `include/idn_voice_client.h`)
//! and JNI; the Rust API is [`Client`].
//!
//! - **Transports**: QUIC on IDN's link (ALPN `idn-voice/1`, connection migration, the
//!   server's link certificate pinned) first; the WebSocket over TLS when QUIC doesn't
//!   connect in `link_connect_ms`; QUIC tried again every `link_retry_s` while on the
//!   WebSocket, moving over when it connects. A connection that ends is resumed (the
//!   newest token and `have`); every timing comes from `hello`'s `link` block.
//! - **Sound**: the app pushes its microphone's PCM ([`Client::push_mic_i16`]) and pulls
//!   the speaker's ([`Client::pull_speaker_i16`]); the library frames 20 ms, codes Opus
//!   (mono, 24 kbit/s, in-band FEC) and sends only while talking; it keeps a jitter buffer
//!   per talker (from `hello`'s and each channel's jitter settings), decodes, conceals
//!   losses and mixes, at 48 kHz mono.
//! - **Threads**: a small tokio runtime of its own (two worker threads); events come on
//!   them. See the README.

mod client;
mod codec;
mod config;
mod conn;
mod event;
pub mod ffi;
mod framing;
mod jitter;
mod mic;
mod outbox;
mod session;
mod speaker;
mod tls;

pub use client::{Client, Stats, Target};
pub use config::{Config, ResumeState, TransportChoice};
pub use event::{Event, Talker, Transport};
pub use framing::{AudioFrame, MAX_FRAME, MAX_FROM_SERVER, MAX_PACKET};

use std::sync::{Mutex, MutexGuard};

/// The speaker's sample rate (Opus's): 48 kHz.
pub const SPEAKER_RATE: u32 = 48_000;
/// Samples in one 20 ms frame at 48 kHz.
pub const FRAME_SAMPLES: usize = 960;
/// A frame's length.
pub const FRAME_MS: u32 = 20;
/// The microphone rates taken as they are (the Opus encoder's own); any other is refused.
pub const MIC_RATES: [u32; 5] = [8_000, 12_000, 16_000, 24_000, 48_000];

/// A lock a panicked holder left is still taken. (`idn_common::sync::LockOk`'s shape: this
/// crate depends on nothing of IDN's own, for the phone builds, as idn-iem.)
pub(crate) trait LockOk<T> {
    fn lock_ok(&self) -> MutexGuard<'_, T>;
}

impl<T> LockOk<T> for Mutex<T> {
    fn lock_ok(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}
