//! The control WebSocket (§V.8): JSON both ways, and on the WebSocket path (the fallback,
//! and handsets' reliable path, §V.13) the sound too (binary messages). A client's binary
//! message is one Opus frame: a kind byte (1) then the packet; what it is sent comes from
//! its `hub::FrameQueue` (and replays as `hub::Out::Frame`). The session itself is
//! `session.rs`'s, shared with IDN's link (§V.14).

use crate::Voice;
use crate::hub::Who;
use crate::session::{Reader, Writer};
use axum::extract::ws::WebSocket;
use futures_util::StreamExt;
use std::net::IpAddr;

/// A connection's life: hello, commands and sound until it closes or is silent for the
/// site's `idle_timeout_s` (30 s). A handset's (§V.13) may come back to its session with
/// `resume`, a token from a `hello`, and `have`, its last `seq` from each talker; its
/// session waits for that when the link goes.
pub async fn session(
    voice: Voice,
    socket: WebSocket,
    who: Who,
    peer: IpAddr,
    resume: Option<String>,
    have: Option<String>,
) {
    let (tx, rx) = socket.split();
    crate::session::run(
        voice,
        (Reader::Ws(rx), Writer::Ws(tx)),
        who,
        peer,
        resume,
        have,
    )
    .await
}
