//! Virtual screens (idn-node.md §N.36): screens with nothing plugged in, drawn by the
//! desktop like any other for a capture to send. On Linux a free connector forced on
//! with an EDID the Node makes ([`linux`], done at the next start by a root helper); on
//! Windows the bundled Virtual Display Driver's settings ([`windows`], at once).

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;
pub mod windows;
