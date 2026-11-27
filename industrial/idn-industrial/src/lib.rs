//! IDN Industrial (spec/extensions/idn-industrial.md): IDN Site Manager | Industrial, the
//! company's central server, and the IDN Node | Industrial's link to it.

pub mod actions;
pub mod bridge;
pub mod chat;
pub mod health;
pub mod http;
/// Injecting keyboard and mouse (shared with IDN remote control, R.25: `idn-input`).
pub use idn_input as input;
/// The HTTP client, the library's (`web::httpc`).
pub use idn_open_protocol::web::httpc;
pub mod local_api;
pub mod modbus;
pub mod modules;
/// The MQTT 5 broker (shared: `idn-mqtt`).
pub use idn_mqtt as mqtt;
pub mod node;
pub mod poll;
pub mod remote;
pub mod screen;
pub mod server;
pub mod session;
pub mod settings;
pub mod sql;
pub mod store;
pub mod tls;
pub mod token;
pub mod transport;
pub mod update;
pub mod wire;
pub mod worker;
