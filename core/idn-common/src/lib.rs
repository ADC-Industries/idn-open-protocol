//! Small pieces every IDN program shares, below the protocol library (which uses them
//! too): saving files safely ([`fsutil`]), where a program keeps its state ([`paths`]),
//! real-time threads for sound ([`rt`]), HTML escaping ([`html`]), JPEG pictures
//! ([`jpeg`]), sizes, on/off and base64 ([`text`]), hex ([`hex`]), `io::Error`s by kind
//! ([`err`]), retry waits ([`backoff`]), the one logger ([`log`]), and running helper
//! programs, started again when they stop ([`supervisor`]). Only the standard library,
//! the system's own interfaces and serde_json (saved files): nothing here is worth a
//! heavy dependency. `STYLE.md` lists the shared helpers and when to use each.

pub mod backoff;
pub mod cmd;
pub mod edid;
pub mod epoch;
pub mod err;
pub mod font5x7;
pub mod fsutil;
pub mod gpu_load;
pub mod heartbeat;
pub mod hex;
pub mod html;
pub mod jpeg;
pub mod log;
pub mod machine;
pub mod memory;
pub mod module;
pub mod net;
pub mod netrings;
pub mod paths;
pub mod pcm;
pub mod ports;
pub mod rt;
pub mod screens;
pub mod shm;
pub mod supervisor;
pub mod switches;
pub mod sync;
#[doc(hidden)]
pub mod testing;
pub mod text;
#[cfg(windows)]
pub mod win;
pub mod worker;
