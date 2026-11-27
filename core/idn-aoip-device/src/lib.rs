//! Being a device on a network of Dante-compatible equipment, IDN's own implementation
//! (roadmap R.31).
//!
//! Unofficial: not made, authorised or supported by Audinate. Dante is Audinate's
//! trademark, named only to say what this works with.
//!
//! Written only from the clean-room specs in `spec/extensions/`: "Being a device"
//! (`idn-aoip-device.md`, cited as §n) and the controller's side
//! (`idn-aoip-control.md`, cited as D6 §n). No other implementation's code was used.
//!
//! The pieces, each pure (bytes in, bytes out) so they are tested without a network:
//! - [`device`]: what a device is: identity, format, channels, names, subscriptions.
//! - [`arc`]: the routing-and-control port's requests and the device's replies (§3).
//! - [`settings`]: the settings port, notifications, heartbeat and CMC (§4).
//! - [`audio`]: the media packet, its samples and timestamps (§5).
//! - [`engine`]: the audio: transmit flows sent, receive flows written, keepalives.
//! - [`flowctl`]: flow control: a receiver's requests for unicast flows and the
//!   transmitter's flow table (§6).
//! - [`flows`]: the flow pages a controller reads, multicast flows it makes (D6 §6.2,
//!   §6.3, §7.7).
//! - [`inventory`], [`control`]: a controller's side (D6): reading another
//!   device's channels, changing its subscriptions.
//! - [`devices`]: the network's devices read for the Site Manager (R.32 stage 1a).
//! - [`writes`]: another device's settings changed (R.32 stage 2), each command offered
//!   once checked on real equipment.
//! - [`mdns`]: the device's services on mDNS, answered and announced (§2).
//!
//! - [`receiver`]: subscribing: which flows to set up, join and stop, each channel's
//!   slot and status (§6.5–§7).
//! - [`server`]: the device on the network: its ports answered, heartbeat and
//!   notifications sent, its mDNS.
//!
//! Copyright ADC Industries. MIT OR Apache-2.0.

pub mod arc;
pub mod audio;
pub mod control;
pub mod device;
pub mod devices;
pub mod engine;
pub mod flowctl;
pub mod flows;
pub mod inventory;
pub mod mdns;
pub mod receiver;
pub mod server;
pub mod settings;
pub mod writes;

/// A big-endian u16 at `at`, if the bytes are there.
pub(crate) fn u16_at(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(d.get(at..at + 2)?.try_into().ok()?))
}

/// A big-endian u32 at `at`, if the bytes are there.
pub(crate) fn u32_at(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(d.get(at..at + 4)?.try_into().ok()?))
}

/// A NUL-terminated ASCII/UTF-8 string at `at`, inside `d`.
pub(crate) fn string_at(d: &[u8], at: usize) -> Option<String> {
    let rest = d.get(at..)?;
    let n = rest.iter().position(|&b| b == 0)?;
    String::from_utf8(rest[..n].to_vec()).ok()
}
