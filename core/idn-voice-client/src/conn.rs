//! One connection to the server, either transport, as two channels: what comes in (JSON
//! text and sound frames, in order) and what goes out. A reader and a writer task each;
//! the connection ends when either does (the incoming channel closes).
//!
//! - **WebSocket**: `GET …/ws` with `Authorization: Bearer CREDENTIAL` (TLS with the pin,
//!   or plain where the site allows plain handsets), `?resume=TOKEN&have=…` to come back.
//! - **QUIC** (the link, `idn-voice/1`): one bidirectional stream; the first frame a JSON
//!   `auth` with the credential, `resume` and `have`.
//!
//! Either way the server's first message is `hello` (or `bye`/`error` and the end).

use crate::config::{Config, ResumeState, parse_url, split_host_port};
use crate::event::Transport;
use crate::framing::{KIND_JSON, KIND_SOUND, MAX_FROM_SERVER, read_frame, write_frame};
use crate::tls;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// A message either way.
#[derive(Debug)]
pub(crate) enum Wire {
    Json(String),
    Sound(Vec<u8>),
}

/// Messages a writer holds before the session waits on it (a stalled link).
const WRITER_QUEUE: usize = 256;

/// A connection that said `hello`.
pub(crate) struct Conn {
    pub transport: Transport,
    pub tx: mpsc::Sender<Wire>,
    pub rx: mpsc::Receiver<Wire>,
    /// The server is the pinned one: QUIC (its pin is required), or TLS whose leaf
    /// certificate matched `cert_sha256` (not a public authority's, not plain).
    pub pinned: bool,
    /// The QUIC connection, to close at once.
    quic: Option<quinn::Connection>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Conn {
    /// Ends it at once (no `bye`: the session waits for a resume, or moved on).
    pub fn drop_now(&mut self) {
        if let Some(q) = &self.quic {
            q.close(0u32.into(), b"");
        }
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl Conn {
    /// What was queued written (a `bye`), within `within`; then the writer ends.
    pub async fn flush(&mut self, within: Duration) {
        let (closed, _) = mpsc::channel(1);
        drop(std::mem::replace(&mut self.tx, closed));
        if let Some(writer) = self.tasks.get_mut(1) {
            let _ = tokio::time::timeout(within, writer).await;
        }
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.drop_now();
    }
}

/// Why a connection didn't come up.
#[derive(Debug)]
pub(crate) enum Refused {
    /// The credential was refused (401/403, or `bye` before `hello`): trying again won't
    /// help.
    Credential(String),
    /// Anything else (the network, a timeout, a certificate): try again later.
    Other(String),
}

fn other(e: impl std::fmt::Display) -> Refused {
    Refused::Other(e.to_string())
}

/// `TALKER:SEQ,…` and the token as a query, percent-encoded.
fn query(resume: Option<&ResumeState>) -> String {
    let Some(r) = resume.filter(|r| !r.token.is_empty()) else {
        return String::new();
    };
    let enc = |s: &str| -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z'
                | b'a'..=b'z'
                | b'0'..=b'9'
                | b'-'
                | b'_'
                | b'.'
                | b'~'
                | b':'
                | b',' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect()
    };
    let mut q = format!("?resume={}", enc(&r.token));
    if !r.have.is_empty() {
        q += &format!("&have={}", enc(&r.have));
    }
    q
}

fn user_agent(cfg: &Config) -> String {
    let me = format!("idn-voice-client/{}", env!("CARGO_PKG_VERSION"));
    // A header takes visible ASCII only: the rest of a name ("Fahrer-Ä") as '?'.
    let ascii = |s: &str| -> String {
        s.chars()
            .map(|c| if (' '..='~').contains(&c) { c } else { '?' })
            .collect()
    };
    match (cfg.app.is_empty(), cfg.app_version.is_empty()) {
        (true, _) => me,
        (false, true) => format!("{} {me}", ascii(&cfg.app)),
        (false, false) => format!("{}/{} {me}", ascii(&cfg.app), ascii(&cfg.app_version)),
    }
}

/// The first message: `hello`, or why not.
fn first(text: &str) -> Result<Value, Refused> {
    let v: Value = serde_json::from_str(text).map_err(other)?;
    match v["t"].as_str() {
        Some("hello") => Ok(v),
        Some("bye") => Err(Refused::Credential(
            v["why"].as_str().unwrap_or("refused").to_owned(),
        )),
        Some("error") => {
            let e = v["error"].as_str().unwrap_or("refused").to_owned();
            // (Anything else, QUIC off at the site say, is a reason to use the WebSocket.)
            if e.starts_with("not an enrolled handset") || e.starts_with("not a credential") {
                Err(Refused::Credential(e))
            } else {
                Err(Refused::Other(e))
            }
        }
        _ => Err(Refused::Other(format!("not a hello first: {text:.200}"))),
    }
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// The WebSocket at `url` (one of the configuration's), with `resume`.
pub(crate) async fn websocket(
    cfg: &Config,
    url: &str,
    resume: Option<&ResumeState>,
) -> Result<(Conn, Value), Refused> {
    let u = parse_url(url).ok_or_else(|| other(format!("not an address: {url}")))?;
    let tcp = tokio::net::TcpStream::connect((u.host.as_str(), u.port))
        .await
        .map_err(|e| other(format!("{url}: {e}")))?;
    // Each sound frame at once (Nagle held them up to 40 ms at the server).
    let _ = tcp.set_nodelay(true);
    let mut pinned = false;
    let stream: Box<dyn Io> = if u.tls {
        let name = rustls::pki_types::ServerName::try_from(u.host.clone()).map_err(other)?;
        let tls = tokio_rustls::TlsConnector::from(
            tls::websocket_config(cfg.cert_sha256).map_err(other)?,
        )
        .connect(name, tcp)
        .await
        .map_err(|e| other(format!("{url}: {e}")))?;
        pinned = cfg.cert_sha256.is_some_and(|pin| {
            tls.get_ref()
                .1
                .peer_certificates()
                .and_then(|c| c.first())
                .is_some_and(|leaf| tls::sha256(leaf.as_ref()) == pin)
        });
        Box::new(tls)
    } else {
        Box::new(tcp)
    };
    let mut req = format!("{url}{}", query(resume))
        .into_client_request()
        .map_err(other)?;
    let h = req.headers_mut();
    h.insert(
        "Authorization",
        format!("Bearer {}", cfg.credential)
            .parse()
            .map_err(other)?,
    );
    h.insert("User-Agent", user_agent(cfg).parse().map_err(other)?);
    // The server's messages at most as long as the link's frames (tungstenite's own
    // default is 64 MiB: a handset's memory).
    let limits = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_FROM_SERVER))
        .max_frame_size(Some(MAX_FROM_SERVER));
    let (ws, _) = match tokio_tungstenite::client_async_with_config(req, stream, Some(limits)).await
    {
        Ok(x) => x,
        Err(tokio_tungstenite::tungstenite::Error::Http(r)) => {
            let status = r.status().as_u16();
            let body = r
                .body()
                .as_ref()
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default();
            let why = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_owned))
                .unwrap_or(body);
            let text = format!("{url}: {status} {why}");
            // 401 is the credential refused; 403 is this address refusing (a handset over
            // plain HTTP where the site wants HTTPS, another site's page): the next one.
            return Err(if status == 401 {
                Refused::Credential(text)
            } else {
                Refused::Other(text)
            });
        }
        Err(e) => return Err(other(format!("{url}: {e}"))),
    };
    let (mut sink, mut stream) = ws.split();
    // The first message is `hello`.
    let hello = loop {
        match stream.next().await {
            Some(Ok(Message::Text(t))) => break first(t.as_str())?,
            Some(Ok(Message::Close(_))) | None => return Err(other(format!("{url}: closed"))),
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(other(format!("{url}: {e}"))),
        }
    };
    let (in_tx, rx) = mpsc::channel(WRITER_QUEUE);
    let (tx, mut out_rx) = mpsc::channel::<Wire>(WRITER_QUEUE);
    let reader = tokio::spawn(async move {
        while let Some(Ok(m)) = stream.next().await {
            let w = match m {
                Message::Text(t) => Wire::Json(t.as_str().to_owned()),
                Message::Binary(b) => Wire::Sound(b.to_vec()),
                Message::Close(_) => break,
                _ => continue,
            };
            if in_tx.send(w).await.is_err() {
                break;
            }
        }
    });
    let writer = tokio::spawn(async move {
        while let Some(w) = out_rx.recv().await {
            let m = match w {
                Wire::Json(s) => Message::text(s),
                Wire::Sound(b) => Message::binary(b),
            };
            if sink.send(m).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });
    Ok((
        Conn {
            transport: Transport::WebSocket,
            tx,
            rx,
            pinned,
            quic: None,
            tasks: vec![reader, writer],
        },
        hello,
    ))
}

/// The client's QUIC endpoints (IPv4 and IPv6), made when first needed and moved to new
/// sockets when the network changes.
#[derive(Default)]
pub(crate) struct Endpoints {
    v4: Option<quinn::Endpoint>,
    v6: Option<quinn::Endpoint>,
}

impl Endpoints {
    fn for_addr(&mut self, addr: &SocketAddr) -> io::Result<quinn::Endpoint> {
        let (slot, bind): (&mut Option<quinn::Endpoint>, SocketAddr) = if addr.is_ipv4() {
            (&mut self.v4, SocketAddr::from(([0, 0, 0, 0], 0)))
        } else {
            (&mut self.v6, SocketAddr::from(([0u16; 8], 0)))
        };
        if let Some(e) = slot {
            return Ok(e.clone());
        }
        let e = quinn::Endpoint::client(bind)?;
        *slot = Some(e.clone());
        Ok(e)
    }

    /// The network changed (Android's connectivity callback): each endpoint on a fresh
    /// socket. A live connection goes on from its new address (connection migration).
    pub fn rebind(&mut self) {
        for (e, bind) in [
            (&self.v4, SocketAddr::from(([0, 0, 0, 0], 0))),
            (&self.v6, SocketAddr::from(([0u16; 8], 0))),
        ] {
            if let Some(e) = e
                && let Ok(s) = std::net::UdpSocket::bind(bind)
            {
                let _ = e.rebind(s);
            }
        }
    }
}

/// QUIC's timings: keep-alive, the idle timeout (the session's), and the time to connect
/// (shared by the addresses).
#[derive(Debug, Clone, Copy)]
pub(crate) struct LinkTimes {
    pub keepalive: Duration,
    pub idle: Duration,
    pub connect: Duration,
}

/// The least an address is given to answer, however many there are.
const ADDRESS_FLOOR: Duration = Duration::from_millis(1_000);

/// Where QUIC goes: the link's addresses (`host:port`, in order) and the SHA-256 its
/// certificate must have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinkTarget {
    pub addrs: Vec<String>,
    pub pin: [u8; 32],
}

impl LinkTarget {
    /// The time all the addresses get: `connect`, or each its least (and a name lookup).
    pub fn budget(&self, connect: Duration) -> Duration {
        connect.max(ADDRESS_FLOOR * self.addrs.len().max(1) as u32) + Duration::from_secs(1)
    }
}

/// QUIC to one of the link's addresses (in order), signed in with `resume`.
pub(crate) async fn quic(
    cfg: &Config,
    to: &LinkTarget,
    endpoints: &std::sync::Mutex<Endpoints>,
    times: LinkTimes,
    resume: Option<&ResumeState>,
) -> Result<(Conn, Value), Refused> {
    use crate::LockOk as _;
    let pin = to.pin;
    let mut last = Refused::Other("no link address".into());
    // Each address its share of the time (a handshake alone waits out the idle timeout):
    // one that never answers (a LAN address from the road, a dead IPv6) leaves the rest
    // their turn.
    let share = (times.connect / to.addrs.len().max(1) as u32).max(ADDRESS_FLOOR);
    for l in &to.addrs {
        let Some((host, port)) = split_host_port(l) else {
            continue;
        };
        let addrs = match tokio::net::lookup_host((host.as_str(), port)).await {
            Ok(a) => a.collect::<Vec<_>>(),
            Err(e) => {
                last = other(format!("{l}: {e}"));
                continue;
            }
        };
        let each = (share / addrs.len().max(1) as u32).max(ADDRESS_FLOOR);
        for addr in addrs {
            let endpoint = endpoints.lock_ok().for_addr(&addr);
            let r = match endpoint {
                Ok(e) => {
                    match tokio::time::timeout(
                        each,
                        quic_one(cfg, &e, addr, &host, pin, times, resume),
                    )
                    .await
                    {
                        Ok(r) => r,
                        Err(_) => Err(other(format!("no answer in {} ms", each.as_millis()))),
                    }
                }
                Err(e) => Err(other(e)),
            };
            match r {
                Ok(c) => return Ok(c),
                Err(Refused::Credential(e)) => return Err(Refused::Credential(e)),
                Err(Refused::Other(e)) => last = Refused::Other(format!("{l}: {e}")),
            }
        }
    }
    Err(last)
}

async fn quic_one(
    cfg: &Config,
    endpoint: &quinn::Endpoint,
    addr: SocketAddr,
    host: &str,
    pin: [u8; 32],
    times: LinkTimes,
    resume: Option<&ResumeState>,
) -> Result<(Conn, Value), Refused> {
    let tls = tls::link_config(pin).map_err(other)?;
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).map_err(other)?;
    let mut qc = quinn::ClientConfig::new(Arc::new(crypto));
    let mut t = quinn::TransportConfig::default();
    t.keep_alive_interval(Some(times.keepalive));
    t.max_idle_timeout(Some(times.idle.try_into().map_err(other)?));
    // Voice uses the one stream the client opens; the server opens none.
    t.max_concurrent_bidi_streams(0u32.into());
    t.max_concurrent_uni_streams(0u32.into());
    qc.transport_config(Arc::new(t));
    // The pin, not the name, is checked; the name only has to be one.
    let name = if rustls::pki_types::ServerName::try_from(host).is_ok() {
        host
    } else {
        "idn-link"
    };
    let conn = endpoint
        .connect_with(qc, addr, name)
        .map_err(other)?
        .await
        .map_err(other)?;
    let (mut send, mut recv) = conn.open_bi().await.map_err(other)?;
    let mut auth = json!({"t": "auth", "credential": cfg.credential});
    if let Some(r) = resume.filter(|r| !r.token.is_empty()) {
        auth["resume"] = json!(r.token);
        if !r.have.is_empty() {
            auth["have"] = json!(r.have);
        }
    }
    if !cfg.app.is_empty() {
        auth["app"] = json!(cfg.app);
        auth["version"] = json!(cfg.app_version);
    }
    write_frame(&mut send, KIND_JSON, auth.to_string().as_bytes())
        .await
        .map_err(other)?;
    let hello = loop {
        match read_frame(&mut recv, MAX_FROM_SERVER).await {
            Ok(Some((KIND_JSON, b))) => break first(&String::from_utf8_lossy(&b))?,
            Ok(Some(_)) => continue,
            Ok(None) => return Err(other("closed before hello")),
            Err(e) => return Err(other(e)),
        }
    };
    let (in_tx, rx) = mpsc::channel(WRITER_QUEUE);
    let (tx, mut out_rx) = mpsc::channel::<Wire>(WRITER_QUEUE);
    let reader = tokio::spawn(async move {
        // A frame too long or cut ends it (a protocol error); another kind is skipped
        // (VOICE-CLIENT.md §12: a later server's).
        while let Ok(Some((kind, b))) = read_frame(&mut recv, MAX_FROM_SERVER).await {
            let w = match kind {
                KIND_JSON => Wire::Json(String::from_utf8_lossy(&b).into_owned()),
                KIND_SOUND => Wire::Sound(b),
                _ => continue,
            };
            if in_tx.send(w).await.is_err() {
                break;
            }
        }
    });
    let writer = tokio::spawn(async move {
        while let Some(w) = out_rx.recv().await {
            let r = match w {
                Wire::Json(s) => write_frame(&mut send, KIND_JSON, s.as_bytes()).await,
                Wire::Sound(b) => write_frame(&mut send, KIND_SOUND, &b).await,
            };
            if r.is_err() {
                break;
            }
        }
        // Ended on purpose (a `bye`): the peer acknowledges what was written before
        // the connection goes ([`Conn::flush`] bounds the wait).
        if send.finish().is_ok() {
            let _ = send.stopped().await;
        }
    });
    Ok((
        Conn {
            transport: Transport::Quic,
            tx,
            rx,
            pinned: true,
            quic: Some(conn),
            tasks: vec![reader, writer],
        },
        hello,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_resume_goes_in_the_query_encoded() {
        assert_eq!(query(None), "");
        let r = ResumeState {
            token: "a b/c".into(),
            have: "17:204,22:9".into(),
        };
        assert_eq!(query(Some(&r)), "?resume=a%20b%2Fc&have=17:204,22:9");
        let r = ResumeState {
            token: "t".into(),
            have: String::new(),
        };
        assert_eq!(query(Some(&r)), "?resume=t");
        let mut c = Config::new("x");
        assert!(user_agent(&c).starts_with("idn-voice-client/"));
        c.app = "Fleet".into();
        c.app_version = "2.4".into();
        assert!(user_agent(&c).starts_with("Fleet/2.4 idn-voice-client/"));
        c.app = "Fahrer-Ä".into();
        c.app_version = "2.4–beta".into();
        assert!(user_agent(&c).starts_with("Fahrer-?/2.4?beta idn-voice-client/"));
        assert!(
            user_agent(&c)
                .parse::<tokio_tungstenite::tungstenite::http::HeaderValue>()
                .is_ok()
        );
        assert!(first(r#"{"t":"hello","sid":1}"#).is_ok());
        assert!(matches!(
            first(r#"{"t":"bye","why":"this handset was revoked"}"#),
            Err(Refused::Credential(_))
        ));
        assert!(matches!(first(r#"{"t":"pong"}"#), Err(Refused::Other(_))));
        assert!(matches!(
            first(
                r#"{"t":"error","what":"auth","error":"not an enrolled handset (revoked, or enrolled again)"}"#
            ),
            Err(Refused::Credential(_))
        ));
        assert!(matches!(
            first(
                r#"{"t":"error","what":"auth","error":"QUIC is off on this site: use the WebSocket"}"#
            ),
            Err(Refused::Other(_))
        ));
    }
}
