//! What the library tells the app: one [`Event`] at a time, from its own threads. Each is
//! also JSON ([`Event::to_json`]) for the C ABI.

use serde_json::{Value, json};

/// The transport a connection runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// QUIC on IDN's link (`idn-voice/1`).
    Quic,
    /// The WebSocket over TLS (TCP).
    WebSocket,
}

impl Transport {
    pub fn name(self) -> &'static str {
        match self {
            Transport::Quic => "quic",
            Transport::WebSocket => "websocket",
        }
    }
}

/// Someone talking in a channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Talker {
    /// Their session (the `talker` of their sound frames).
    pub sid: u32,
    pub name: String,
}

/// An event.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Connected (or moved over) on this transport; a `Hello` follows.
    Connected(Transport),
    /// The connection ended; the library connects again (`attempt` counts the tries since
    /// the last connection, 0 at once after a drop).
    Reconnecting { attempt: u32, why: String },
    /// The server's `hello` (VOICE-CLIENT.md §4.2), whole. `resumed`: the session went on
    /// (channels kept, missed sound coming); otherwise a fresh one.
    Hello { resumed: bool, hello: Value },
    /// The handset's driver changed (`hello.me.driver`: `{"id", "name"}`, or none).
    Driver { driver: Option<Value> },
    /// In a channel (joined, or put there by a driver's assignment).
    Joined { channel: u32, may_talk: bool },
    /// Out of a channel (`why`: `left`, `driver changed`, …).
    Left { channel: u32, why: String },
    /// Who talks in a channel now (empty: nobody).
    Talking { channel: u32, talkers: Vec<Talker> },
    /// This client's own talk started or ended (`why` when ended: `released`, `time
    /// limit`, `taken`, …), or was refused (`busy`).
    Talk { on: bool, why: Option<String> },
    /// A text message (the MESSAGE object).
    Message(Value),
    /// `sos` (dispatch), `sos_sent` (this handset's reached the server) or `sos_ack`: the
    /// server's message, whole.
    Sos(Value),
    /// A command refused (`what`: the command) or a connection's error.
    Error { what: Option<String>, error: String },
    /// The session ended for good: `bye` from the server (revoked, taken over by another
    /// connection of this handset), or the credential refused. The library stops; a new
    /// [`Client`](crate::Client) is needed (after enrolling again, for a refused one).
    /// ([`Client::close`](crate::Client::close) sends none: no event follows it.)
    Closed { why: String },
    /// Any other server message, whole (`members`, `presence`, `ring`, `call`, …).
    Other(Value),
}

impl Event {
    /// As the C ABI delivers it: `{"event": NAME, …}`.
    pub fn to_json(&self) -> Value {
        match self {
            Event::Connected(t) => json!({"event": "connected", "transport": t.name()}),
            Event::Reconnecting { attempt, why } => {
                json!({"event": "reconnecting", "attempt": attempt, "why": why})
            }
            Event::Hello { resumed, hello } => {
                json!({"event": "hello", "resumed": resumed, "hello": hello})
            }
            Event::Driver { driver } => json!({"event": "driver", "driver": driver}),
            Event::Joined { channel, may_talk } => {
                json!({"event": "joined", "channel": channel, "may_talk": may_talk})
            }
            Event::Left { channel, why } => {
                json!({"event": "left", "channel": channel, "why": why})
            }
            Event::Talking { channel, talkers } => json!({
                "event": "talking", "channel": channel,
                "talkers": talkers.iter().map(|t| json!({"sid": t.sid, "name": t.name})).collect::<Vec<_>>(),
            }),
            Event::Talk { on, why } => json!({"event": "talk", "on": on, "why": why}),
            Event::Message(m) => json!({"event": "message", "message": m}),
            Event::Sos(m) => json!({"event": "sos", "message": m}),
            Event::Error { what, error } => {
                json!({"event": "error", "what": what, "error": error})
            }
            Event::Closed { why } => json!({"event": "closed", "why": why}),
            Event::Other(m) => json!({"event": "other", "message": m}),
        }
    }
}
