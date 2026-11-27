//! The library's integration tests in one program (as idn-voice's): against the real
//! Voice server in-process over the WebSocket, against a QUIC server of the tests' own
//! for the link's framing, and (behind `quic-server-tests`) against the real server's QUIC.

mod common;
mod quic;
#[cfg(feature = "quic-server-tests")]
mod quic_server;
mod websocket;
