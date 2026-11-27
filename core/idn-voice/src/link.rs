//! Voice over IDN's link (§V.14): QUIC with connection migration, ALPN `idn-voice/1`,
//! for handsets that move between networks. The client opens one stream both ways; each
//! direction is frames of a u32 little-endian length (of what follows), a kind byte (0:
//! the WebSocket's JSON, 1: its binary sound message) and the payload. The first frame
//! is the client's `auth`; then it is the WebSocket's session (`session.rs`), the same in
//! every way.

use crate::Voice;
use crate::session::{Reader, Writer};
use idn_open_protocol::link::{LinkConfig, ServiceListener, ServicePending, ServiceTransport};
use serde::Serialize;
use serde_json::{Value, json};
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Weak};
use std::time::Duration;

/// The ALPN Voice is offered under.
pub const ALPN: &str = "idn-voice/1";
/// The largest frame to the server (its kind byte and payload); a longer one ends the
/// connection.
pub const MAX_FRAME: usize = 64 * 1024;
/// The largest frame from the server (its kind byte and payload): a `history`, `missed`,
/// or a large site's `presence` or `hello`, passes 64 KiB. The server ends a session
/// rather than write a longer one; its WebSocket messages are held to the same.
pub const MAX_FROM_SERVER: usize = 4 * 1024 * 1024;
pub(crate) const KIND_JSON: u8 = 0;
pub(crate) const KIND_SOUND: u8 = 1;
/// CONNECTION_CLOSE codes: done, refused (the reason says why), a protocol error (as
/// the link's, idn-extensions.md §E.11.6).
pub(crate) const DONE: u32 = 0;
const REFUSED: u32 = 1;
pub(crate) const PROTOCOL: u32 = 2;
/// The stream and the `auth` frame come within this long, both together (§V.14).
const AUTH_WITHIN: Duration = Duration::from_secs(10);
/// What a client may send ahead of Voice's reading: one stream, two of the longest frames
/// on it (each with its length), 256 KiB on the connection. quinn's defaults (four
/// streams of 1.25 MB, the connection unbounded) let a client that never signs in make
/// the server hold megabytes.
const STREAM_WINDOW: u32 = 2 * (MAX_FRAME as u32 + 4);
const RECEIVE_WINDOW: u32 = 256 << 10;
/// How long a closing connection waits for the client to close it, having read the last
/// frame (an error, a `bye`).
const LINGER: Duration = Duration::from_secs(2);

/// Where Voice's QUIC is offered: told to clients in `hello` and at enrolment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LinkOffer {
    /// The UDP port.
    pub port: u16,
    /// Addresses (`host:port`) from the server's configured ones (`IDN_VOICE_URLS`):
    /// clients try the host they reached Voice by first, at `port`.
    pub addrs: Vec<String>,
    /// The SHA-256 of the listener's certificate, `AA:BB:…`: what clients pin.
    pub cert_sha256: String,
}

/// Voice's link listener; dropped, it stops and Voice's QUIC is no longer offered (the
/// offer cleared only if it is still this listener's).
pub struct VoiceLink {
    listener: ServiceListener,
    hub: Weak<crate::hub::Hub>,
    /// The offer it made (`Hub::set_link_offer`'s number).
    offer: u64,
}

impl VoiceLink {
    /// The UDP port listened on.
    pub fn port(&self) -> u16 {
        self.listener.port()
    }

    /// The certificate's SHA-256, `AA:BB:…`.
    pub fn cert_sha256(&self) -> String {
        crate::model::fingerprint_text(self.listener.cert_sha256()).unwrap_or_default()
    }
}

impl Drop for VoiceLink {
    fn drop(&mut self) {
        if let Some(h) = self.hub.upgrade() {
            h.clear_link_offer(self.offer);
        }
    }
}

/// `host:port` for each address's host (`https://H[:P]/…`), at `port`; repeats left out.
pub fn addrs<'a>(port: u16, urls: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for u in urls {
        let rest = u
            .strip_prefix("https://")
            .or_else(|| u.strip_prefix("http://"))
            .unwrap_or(u);
        let authority = rest.split('/').next().unwrap_or_default();
        let (host, _) = idn_open_protocol::web::split_host(authority);
        if host.is_empty() {
            continue;
        }
        let a = if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        if !out.contains(&a) {
            out.push(a);
        }
    }
    out
}

/// One frame from the server: its length, its kind, the payload. Longer than
/// [`MAX_FROM_SERVER`]: `InvalidInput`, never written.
pub(crate) fn frame(kind: u8, payload: &[u8]) -> io::Result<Vec<u8>> {
    if payload.len() + 1 > MAX_FROM_SERVER {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "a frame of {} bytes (at most {MAX_FROM_SERVER})",
                payload.len() + 1
            ),
        ));
    }
    let mut f = Vec::with_capacity(5 + payload.len());
    f.extend_from_slice(&(payload.len() as u32 + 1).to_le_bytes());
    f.push(kind);
    f.extend_from_slice(payload);
    Ok(f)
}

/// A [`Voice`] that doesn't keep it running: what the listener holds, so a Site Manager
/// that holds the listener and is held by Voice (Industrial: its host) makes no cycle,
/// and Voice's store is closed (its last writes made) when the Site Manager lets it go.
struct WeakVoice {
    hub: Weak<crate::hub::Hub>,
    feeds: Weak<crate::feeds::Feeds>,
    cfg: Arc<crate::VoiceConfig>,
    logins: Arc<crate::http::Logins>,
    rtc: Option<crate::rtc::RtcHandle>,
}

impl WeakVoice {
    fn of(v: &Voice) -> WeakVoice {
        WeakVoice {
            hub: Arc::downgrade(&v.hub),
            feeds: Arc::downgrade(&v.feeds),
            cfg: v.cfg.clone(),
            logins: v.logins.clone(),
            rtc: v.rtc.clone(),
        }
    }

    /// Voice, while something else still holds it.
    fn upgrade(&self) -> Option<Voice> {
        Some(Voice {
            hub: self.hub.upgrade()?,
            feeds: self.feeds.upgrade()?,
            cfg: self.cfg.clone(),
            logins: self.logins.clone(),
            rtc: self.rtc.clone(),
        })
    }
}

/// The next frame (kind, payload); `None` at the stream's end. Too long or empty:
/// `InvalidData`.
pub(crate) async fn read_frame(r: &mut quinn::RecvStream) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(e) => return Err(idn_common::err::other(e)),
    }
    let n = u32::from_le_bytes(len) as usize;
    if n == 0 || n > MAX_FRAME {
        return Err(idn_common::err::invalid_data(format!(
            "a frame of {n} bytes (1 to {MAX_FRAME})"
        )));
    }
    let mut b = vec![0u8; n];
    r.read_exact(&mut b).await.map_err(idn_common::err::other)?;
    let kind = b.remove(0);
    Ok(Some((kind, b)))
}

impl Voice {
    /// Serves Voice over IDN's link on UDP `addr` (Media: the HTTPS port's number, as the
    /// link takes its TCP port's), presenting `link`'s key; connections run on `rt`. Hold
    /// what it returns: dropped, the listener stops. Offered to clients while the site's
    /// `link` setting is on.
    pub fn serve_link(
        &self,
        addr: SocketAddr,
        link: &LinkConfig,
        rt: &tokio::runtime::Handle,
    ) -> io::Result<VoiceLink> {
        // (Weak: the listener and its tasks must not keep Voice alive. They held it
        // strongly, and in Industrial the Site Manager holding the listener is held by
        // Voice's host: a cycle, and Voice's last writes never made at exit.)
        let hub = Arc::downgrade(&self.hub);
        let transport = move || {
            let s = hub.upgrade().map(|h| h.settings()).unwrap_or_default();
            ServiceTransport {
                keep_alive: Duration::from_millis(s.link_keepalive_ms as u64),
                idle_timeout: Duration::from_secs(s.idle_timeout_s as u64),
                streams: 1,
                stream_window: STREAM_WINDOW,
                receive_window: RECEIVE_WINDOW,
            }
        };
        let (weak, rt) = (WeakVoice::of(self), rt.clone());
        let listener =
            ServiceListener::bind(addr, link, &[ALPN], transport, move |_, conn, pending| {
                // Voice let go (its Site Manager stopping): no new sessions.
                let Some(v) = weak.upgrade() else {
                    conn.close(REFUSED.into(), b"Voice is stopping");
                    return;
                };
                rt.spawn(async move {
                    v.serve_link_conn(conn, Some(pending)).await;
                    // (Voice may have been let go meanwhile: its last handle is dropped
                    // off the runtime's workers, as its store's drop waits for its writer
                    // thread.)
                    drop(tokio::task::spawn_blocking(move || drop(v)));
                });
            })?;
        let offer = self
            .hub
            .set_link_offer(self.link_offer_at(Some((listener.port(), listener.cert_sha256()))));
        Ok(VoiceLink {
            listener,
            hub: Arc::downgrade(&self.hub),
            offer,
        })
    }

    /// Tells clients where Voice's QUIC is: a UDP port and the SHA-256 of the certificate
    /// there (hex, with or without colons); `None`: not offered. For a host whose own link
    /// listener hands `idn-voice/1` connections to [`Voice::serve_link_connection`].
    pub fn offer_link(&self, at: Option<(u16, &str)>) {
        self.hub.set_link_offer(self.link_offer_at(at));
    }

    fn link_offer_at(&self, at: Option<(u16, &str)>) -> Option<LinkOffer> {
        at.and_then(|(port, fp)| {
            Some(LinkOffer {
                port,
                addrs: addrs(port, self.cfg.urls.iter().map(String::as_str)),
                cert_sha256: crate::model::fingerprint_text(fp)?,
            })
        })
    }

    /// Serves one accepted QUIC connection of ALPN `idn-voice/1`: its stream, the `auth`
    /// frame (a handset's credential, or the site program's Bearer token; with a handset's
    /// `resume` and `have`), then the session, as on the WebSocket. The host's own
    /// listener should bound what an unauthenticated client may send and how many such
    /// connections it holds, as [`Voice::serve_link`]'s does.
    pub async fn serve_link_connection(&self, conn: quinn::Connection) {
        self.serve_link_conn(conn, None).await
    }

    /// As [`Voice::serve_link_connection`]; `pending` (the connection counted among
    /// those not yet signed in) is let go once the client has signed in.
    async fn serve_link_conn(&self, conn: quinn::Connection, pending: Option<ServicePending>) {
        let peer = conn.remote_address().ip().to_canonical();
        // One deadline for the stream and the auth frame (each had 10 s of its own).
        let deadline = tokio::time::Instant::now() + AUTH_WITHIN;
        let Ok(Ok((mut send, mut recv))) =
            tokio::time::timeout_at(deadline, conn.accept_bi()).await
        else {
            conn.close(DONE.into(), b"no stream");
            return;
        };
        let auth = match tokio::time::timeout_at(deadline, read_frame(&mut recv)).await {
            Ok(Ok(Some((KIND_JSON, p)))) => serde_json::from_slice::<Value>(&p).ok(),
            Ok(Err(e)) if e.kind() == io::ErrorKind::InvalidData => {
                conn.close(PROTOCOL.into(), e.to_string().as_bytes());
                return;
            }
            _ => None,
        };
        let Some(auth) = auth.filter(|a| a["t"] == "auth") else {
            return refuse(&conn, &mut send, "the first frame is auth").await;
        };
        if !self.hub.settings().link {
            return refuse(
                &conn,
                &mut send,
                "QUIC is off on this site: use the WebSocket",
            )
            .await;
        }
        let credential = auth["credential"].as_str().unwrap_or_default().trim();
        let who = if credential.starts_with(crate::hub::CREDENTIAL_PREFIX) {
            match self.hub.handset_by_credential(credential) {
                Some(h) => self.hub.handset_who(&h),
                None => {
                    let why = "not an enrolled handset (revoked, or enrolled again)";
                    return refuse(&conn, &mut send, why).await;
                }
            }
        } else {
            // The site's program (Industrial's API key), as its Bearer on the WebSocket.
            // No cookies or guests over the link.
            let mut h = axum::http::HeaderMap::new();
            let user = match axum::http::HeaderValue::from_str(&format!("Bearer {credential}")) {
                Ok(b) if !credential.is_empty() => {
                    h.insert(axum::http::header::AUTHORIZATION, b);
                    self.hub.host.session_user(&h)
                }
                _ => None,
            };
            match user {
                Some(u) => crate::hub::Who {
                    api: self.hub.host.is_program(&h),
                    ..crate::http::who_of(&u)
                },
                None => return refuse(&conn, &mut send, "not a credential of this site").await,
            }
        };
        let resume = auth["resume"]
            .as_str()
            .map(str::to_owned)
            .filter(|_| who.handset.is_some());
        let have = auth["have"]
            .as_str()
            .map(str::to_owned)
            .filter(|_| resume.is_some());
        // Signed in: no longer counted among the connections still proving who they are.
        drop(pending);
        crate::session::run(
            self.clone(),
            (
                Reader::Link(recv, conn.clone()),
                Writer::Link(send, conn.clone()),
            ),
            who,
            peer,
            resume,
            have,
        )
        .await;
        // (A `bye` sent: read before the close, as an error is.)
        let _ = tokio::time::timeout(LINGER, conn.closed()).await;
        conn.close(DONE.into(), b"done");
    }
}

/// An `error` frame, then the connection closed.
async fn refuse(conn: &quinn::Connection, send: &mut quinn::SendStream, why: &str) {
    let e = json!({"t": "error", "what": "auth", "error": why});
    let f = frame(KIND_JSON, e.to_string().as_bytes()).unwrap_or_default();
    if tokio::time::timeout(LINGER, send.write_all(&f))
        .await
        .is_ok_and(|r| r.is_ok())
    {
        let _ = send.finish();
        // The client closes once it has read it: a close from here first could lose it.
        let _ = tokio::time::timeout(LINGER, conn.closed()).await;
    }
    conn.close(REFUSED.into(), why.as_bytes());
}
