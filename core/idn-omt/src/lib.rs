//! Open Media Transport (OMT, openmediatransport.org) for IDN (spec/extensions/
//! idn-omt.md; vMix F9): OMT's wire format and its sources and receivers written in Rust
//! after the protocol as libomtnet implements it (MIT), with its video codec, VMX, from
//! libvmx (MIT, vendored and compiled in). The bridge (`idn omt`) offers OMT sources as
//! IDN sources and IDN sources as OMT sources; the IDN Node runs it (`omt.sends`,
//! `omt.recvs`).
//!
//! Parts derived from libomtnet and libvmx: Copyright (c) 2025 Open Media Transport
//! Contributors, MIT licence (vendor/libvmx/LICENSE.txt).

pub mod program;
pub mod receiver;
pub mod sender;
pub mod vmx;
pub mod wire;

pub use receiver::{Event, Picture, Receiver};
pub use sender::{Sender, SenderConfig};
