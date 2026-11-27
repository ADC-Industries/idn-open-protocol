//! The receiver's public types: its errors, trust, transport, configuration, events and
//! status.

use super::*;

/// What can go wrong.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// The configuration doesn't hold (a bad address, buffer bounds, …).
    Config(String),
    /// The network (connecting, sending, the desk's port closed).
    Io(String),
    /// TLS: the desk's certificate isn't the one trusted, or the handshake failed.
    Tls(String),
    /// The desk's API answered with this status.
    Http(u16, String),
    /// From the network a token is needed (REPLY TOKEN_NEEDED, HTTP 401/403 without one).
    TokenNeeded(String),
    /// The token was refused: forged, expired, another site's or engine's.
    TokenRefused(String),
    /// The desk sends to as many phones as it may.
    Full(String),
    /// Too many requests: the library tries again by itself.
    Busy(String),
    /// The token's scope doesn't allow it (a bus token, another mix).
    NotAllowed(String),
    /// The desk didn't understand the request.
    Bad(String),
    /// The desk sends no Opus flow (`--no-iem-net`, or no UDP port).
    NoFlow,
    /// Multicast asked for, and the desk sends none.
    NoMulticast,
    /// The mix isn't on the desk (or none was named, with a token not for one mix).
    NoMix(String),
    /// Not a channel of the flow.
    UnknownChannel(u16),
    /// No answer in time.
    Timeout,
    /// The desk refused a change of the mix.
    Desk(String),
    /// The receiver is closed.
    Closed,
}

impl Error {
    /// A short name for the kind (the C ABI's codes, events' `error`).
    pub fn kind(&self) -> &'static str {
        match self {
            Error::Config(_) => "config",
            Error::Io(_) => "io",
            Error::Tls(_) => "tls",
            Error::Http(..) => "http",
            Error::TokenNeeded(_) => "token_needed",
            Error::TokenRefused(_) => "token_refused",
            Error::Full(_) => "full",
            Error::Busy(_) => "busy",
            Error::NotAllowed(_) => "not_allowed",
            Error::Bad(_) => "bad",
            Error::NoFlow => "no_flow",
            Error::NoMulticast => "no_multicast",
            Error::NoMix(_) => "no_mix",
            Error::UnknownChannel(_) => "unknown_channel",
            Error::Timeout => "timeout",
            Error::Desk(_) => "desk",
            Error::Closed => "closed",
        }
    }

    pub(super) fn of_reply(status: Reply, msg: &str) -> Error {
        let m = msg.to_owned();
        match status {
            Reply::TokenNeeded => Error::TokenNeeded(m),
            Reply::TokenRefused => Error::TokenRefused(m),
            Reply::Full => Error::Full(m),
            Reply::Busy => Error::Busy(m),
            Reply::NotAllowed => Error::NotAllowed(m),
            _ => Error::Bad(m),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Config(m) => write!(f, "configuration: {m}"),
            Error::Io(m) => write!(f, "network: {m}"),
            Error::Tls(m) => write!(f, "TLS: {m}"),
            Error::Http(s, m) => write!(f, "the desk answered {s}: {m}"),
            Error::TokenNeeded(m) => write!(f, "a token is needed: {m}"),
            Error::TokenRefused(m) => write!(f, "the token was refused: {m}"),
            Error::Full(m) => write!(f, "the desk is full: {m}"),
            Error::Busy(m) => write!(f, "the desk is busy: {m}"),
            Error::NotAllowed(m) => write!(f, "not allowed: {m}"),
            Error::Bad(m) => write!(f, "refused: {m}"),
            Error::NoFlow => write!(f, "the desk sends no IEM flow for phones"),
            Error::NoMulticast => write!(f, "the desk sends no multicast"),
            Error::NoMix(m) => write!(f, "no mix: {m}"),
            Error::UnknownChannel(c) => write!(f, "channel {c} is not in the IEM flow"),
            Error::Timeout => write!(f, "no answer in time"),
            Error::Desk(m) => write!(f, "the desk refused the change: {m}"),
            Error::Closed => write!(f, "closed"),
        }
    }
}

impl std::error::Error for Error {}

/// How the desk's network port (HTTPS) is trusted.
#[derive(Debug, Clone, PartialEq)]
pub enum Trust {
    /// A certificate (DER): the desk's own, or the site CA's that signed it.
    Certificate(Vec<u8>),
    /// The desk's certificate's SHA-256 fingerprint (hex, colons allowed), as the desk
    /// says it at its start.
    Sha256(String),
}

impl Trust {
    /// A certificate file's contents: DER, or PEM (its first certificate).
    pub fn from_file_bytes(b: &[u8]) -> Result<Trust, Error> {
        use rustls::pki_types::pem::PemObject;
        if b.starts_with(b"-----") || b.iter().take(64).all(|c| c.is_ascii()) {
            let der = rustls::pki_types::CertificateDer::from_pem_slice(b)
                .map_err(|e| Error::Config(format!("certificate: {e}")))?;
            return Ok(Trust::Certificate(der.to_vec()));
        }
        Ok(Trust::Certificate(b.to_vec()))
    }
}

/// How the flow comes: the phone picks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Multicast when the desk sends it and audio comes that way within
    /// [`Config::multicast_wait`]; else (and if it stops) unicast.
    Auto,
    /// A stream of its own (counts against the desk's `max_phones`).
    Unicast,
    /// The desk's group (one stream for every phone; needs a Wi-Fi that carries it).
    Multicast,
}

impl Transport {
    pub fn name(self) -> &'static str {
        match self {
            Transport::Auto => "auto",
            Transport::Unicast => "unicast",
            Transport::Multicast => "multicast",
        }
    }

    pub fn parse(s: &str) -> Option<Transport> {
        match s {
            "auto" => Some(Transport::Auto),
            "unicast" => Some(Transport::Unicast),
            "multicast" => Some(Transport::Multicast),
            _ => None,
        }
    }
}

/// What to connect to, and how.
#[derive(Debug, Clone)]
pub struct Config {
    /// The desk: `HOST[:PORT]` (no port: 29214 plain, the desk's own machine; 29215 with
    /// `trust`, the network).
    pub desk: String,
    /// A site token for the desk (`iem:<mix>` usually; `control` or `read` too); empty
    /// only on the desk's own machine.
    pub token: String,
    /// The mix to follow: the token's own with an `iem:` token (may be left out); needed
    /// with a `control` or `read` token or none.
    pub mix: Option<String>,
    /// HTTPS to the desk's network port, its certificate pinned; `None`: plain HTTP (the
    /// desk's own machine).
    pub trust: Option<Trust>,
    pub transport: Transport,
    /// In [`Transport::Auto`], how long to wait for multicast audio before unicast.
    pub multicast_wait: Duration,
    /// The interface to join the group on (`None`: the system's choice).
    pub multicast_interface: Option<Ipv4Addr>,
    /// Ask to talk back (push-to-talk). A token that may not talk keeps listening.
    pub talk: bool,
    /// The jitter buffer: its start (and first target), and its bounds (ms).
    pub buffer_ms: f32,
    pub min_buffer_ms: f32,
    pub max_buffer_ms: f32,
    /// The desk's UDP port to use instead of the description's (a port forwarded).
    pub udp_port: Option<u16>,
    /// How long [`Receiver::connect`] waits for the desk.
    pub connect_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            desk: String::new(),
            token: String::new(),
            mix: None,
            trust: None,
            transport: Transport::Auto,
            multicast_wait: Duration::from_millis(1500),
            multicast_interface: None,
            talk: false,
            buffer_ms: 15.0,
            min_buffer_ms: 10.0,
            max_buffer_ms: 100.0,
            udp_port: None,
            connect_timeout: Duration::from_secs(5),
        }
    }
}

impl Config {
    /// From JSON (the C ABI's): `desk`, `token`, `mix`, `transport` (`auto`, `unicast`,
    /// `multicast`), `ca_sha256`, `talk`, `buffer_ms`, `min_buffer_ms`, `max_buffer_ms`,
    /// `multicast_wait_ms`, `multicast_interface`, `udp_port`, `connect_timeout_ms`. A
    /// certificate to pin comes beside it ([`Trust::Certificate`]).
    pub fn from_json(v: &Value) -> Result<Config, Error> {
        let d = Config::default();
        let bad = |k: &str| Error::Config(format!("{k}: {}", v[k]));
        let f = |k: &str, def: f32| -> Result<f32, Error> {
            match &v[k] {
                Value::Null => Ok(def),
                x => x.as_f64().map(|x| x as f32).ok_or_else(|| bad(k)),
            }
        };
        let ms = |k: &str, def: Duration| -> Result<Duration, Error> {
            match &v[k] {
                Value::Null => Ok(def),
                x => x.as_u64().map(Duration::from_millis).ok_or_else(|| bad(k)),
            }
        };
        Ok(Config {
            desk: v["desk"].as_str().ok_or_else(|| bad("desk"))?.to_owned(),
            token: v["token"].as_str().unwrap_or_default().to_owned(),
            mix: v["mix"]
                .as_str()
                .filter(|m| !m.is_empty())
                .map(str::to_owned),
            trust: v["ca_sha256"].as_str().map(|s| Trust::Sha256(s.to_owned())),
            transport: match &v["transport"] {
                Value::Null => d.transport,
                x => x
                    .as_str()
                    .and_then(Transport::parse)
                    .ok_or_else(|| bad("transport"))?,
            },
            multicast_wait: ms("multicast_wait_ms", d.multicast_wait)?,
            multicast_interface: match &v["multicast_interface"] {
                Value::Null => None,
                x => Some(
                    x.as_str()
                        .and_then(|s| s.parse().ok())
                        .ok_or_else(|| bad("multicast_interface"))?,
                ),
            },
            talk: v["talk"].as_bool().unwrap_or(false),
            buffer_ms: f("buffer_ms", d.buffer_ms)?,
            min_buffer_ms: f("min_buffer_ms", d.min_buffer_ms)?,
            max_buffer_ms: f("max_buffer_ms", d.max_buffer_ms)?,
            udp_port: match &v["udp_port"] {
                Value::Null => None,
                x => Some(
                    x.as_u64()
                        .and_then(|p| u16::try_from(p).ok())
                        .ok_or_else(|| bad("udp_port"))?,
                ),
            },
            connect_timeout: ms("connect_timeout_ms", d.connect_timeout)?,
        })
    }
}

/// What happened (in order; [`Receiver::next_event`], or a callback).
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The desk took the session (again).
    Connected {
        transport: Transport,
    },
    /// The desk stopped answering, or ended the session: trying again.
    Disconnected {
        reason: String,
    },
    /// Unicast after multicast (or back), and why.
    TransportChanged {
        transport: Transport,
        reason: String,
    },
    /// The push channel is up (the mix followed live) or down (trying again).
    PushConnected,
    PushDisconnected {
        reason: String,
    },
    /// The mix changed on the desk (the sound person, or the musician elsewhere).
    MixChanged,
    /// The flow's channels changed (marked, unmarked, renamed, paired).
    ChannelsChanged,
    /// The token may not talk back: listening only.
    TalkRefused {
        reason: String,
    },
    Error(Error),
}

impl Event {
    pub fn to_json(&self) -> Value {
        match self {
            Event::Connected { transport } => {
                json!({"type": "connected", "transport": transport.name()})
            }
            Event::Disconnected { reason } => json!({"type": "disconnected", "reason": reason}),
            Event::TransportChanged { transport, reason } => {
                json!({"type": "transport", "transport": transport.name(), "reason": reason})
            }
            Event::PushConnected => json!({"type": "push_connected"}),
            Event::PushDisconnected { reason } => {
                json!({"type": "push_disconnected", "reason": reason})
            }
            Event::MixChanged => json!({"type": "mix_changed"}),
            Event::ChannelsChanged => json!({"type": "channels_changed"}),
            Event::TalkRefused { reason } => json!({"type": "talk_refused", "reason": reason}),
            Event::Error(e) => {
                json!({"type": "error", "error": e.kind(), "message": e.to_string()})
            }
        }
    }
}

/// How it goes, now.
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    /// The desk answers the session and audio came within the last second.
    pub connected: bool,
    /// The desk answers the session.
    pub session: bool,
    /// The push channel is up (the mix followed live).
    pub push: bool,
    /// How the audio comes now (`Unicast` or `Multicast`), and what was asked.
    pub transport: Transport,
    pub asked: Transport,
    /// The jitter buffer: what it holds on average, and its target (ms).
    pub buffer_ms: f32,
    pub target_ms: f32,
    /// Packets lost lately (%, about the last second).
    pub loss_pct: f32,
    /// The session's round trip (KEEPALIVE to REPLY), ms.
    pub rtt_ms: Option<f32>,
    /// The desk's part of the latency (its block, conversion, frame, Opus look-ahead).
    pub desk_ms: f32,
    /// The sound output's, as the app (or `play`) said it.
    pub output_ms: f32,
    /// Desk to ear: the desk's part, half the round trip, the buffer and the output.
    pub latency_ms: f32,
    pub talk_allowed: bool,
    pub talking: bool,
    /// Channels decoded (those in the mix).
    pub channels: usize,
    pub counters: Counters,
    /// SUBSCRIBEs sent (the first, renewals, after UNKNOWN).
    pub subscribes: u64,
    /// The last error, if any since connecting.
    pub error: Option<String>,
    pub desk: String,
    pub mix: Option<String>,
}

impl Status {
    pub fn to_json(&self) -> Value {
        let c = &self.counters;
        let r1 = |v: f32| (f64::from(v) * 10.0).round() / 10.0;
        json!({
            "connected": self.connected, "session": self.session, "push": self.push,
            "transport": self.transport.name(), "asked": self.asked.name(),
            "buffer_ms": r1(self.buffer_ms), "target_ms": r1(self.target_ms),
            "loss_pct": r1(self.loss_pct), "rtt_ms": self.rtt_ms.map(r1),
            "desk_ms": r1(self.desk_ms), "output_ms": r1(self.output_ms),
            "latency_ms": r1(self.latency_ms),
            "talk_allowed": self.talk_allowed, "talking": self.talking,
            "channels": self.channels,
            "counters": {"packets": c.packets, "duplicates": c.duplicates, "late": c.late,
                         "lost": c.lost, "concealed": c.concealed, "underruns": c.underruns,
                         "drops": c.drops, "skips": c.skips, "restarts": c.restarts},
            "subscribes": self.subscribes,
            "error": self.error, "desk": self.desk, "mix": self.mix,
        })
    }
}
