//! IDN Node's service (spec/extensions/idn-node.md): the Node itself ([`server`]), the
//! processes its settings start ([`supervisor`]) and updates from the Site Manager
//! ([`update`]). What every program uses to talk to its Node, and the Node's settings,
//! are in the library (`idn_open_protocol::node`).
// The Node's status is one `json!` of every setting: past the default 128 levels.
#![recursion_limit = "256"]

#[doc(hidden)]
pub mod awake;
#[doc(hidden)]
pub mod drivers;
pub mod host;
#[doc(hidden)]
pub mod ipc;
pub mod media;
pub mod pairs;
#[doc(hidden)]
pub mod panel;
pub mod panel_state;
#[doc(hidden)]
pub mod server;
#[doc(hidden)]
pub mod service;
#[doc(hidden)]
pub mod virtual_screen;
/// The services' supervisor, shared with `idn session` ([`idn_common::supervisor`]).
pub use idn_common::supervisor;
#[cfg(windows)]
pub mod session_agent;
pub mod update;
