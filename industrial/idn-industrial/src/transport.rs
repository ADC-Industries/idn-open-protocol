//! The link's two transports (§I.4): QUIC, and a WebSocket carrying IDN's own TLS with
//! yamux streams inside it, for networks (and Cloudflare Tunnels) that pass only HTTPS.
//! Above this module both are the same: streams both ways, the ALPN negotiated and the
//! peer's certificate.

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use idn_common::sync::LockOk as _;
use rustls::pki_types::{CertificateDer, ServerName};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};

/// One half of a stream.
pub type Send = Box<dyn AsyncWrite + std::marker::Send + Unpin>;
pub type Recv = Box<dyn AsyncRead + std::marker::Send + Unpin>;

/// The WebSocket subprotocol (§I.4.2).
pub const WS_PROTOCOL: &str = "idn-industrial.1";
/// The path the Site Manager serves the WebSocket link on.
pub const WS_PATH: &str = "/idn/link";

/// How long a connection attempt may take.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

use idn_common::err::other as err;

/// An established link, either transport.
#[derive(Clone)]
pub struct Link {
    inner: Inner,
    /// The ALPN agreed (`idn-industrial/1` or `idn-industrial-join/1`).
    pub alpn: Vec<u8>,
    /// The peer's certificate chain, if it presented one.
    pub peer_certs: Option<Vec<CertificateDer<'static>>>,
    /// `quic` or `websocket`.
    pub transport: &'static str,
    /// The peer's address as seen here (for a WebSocket behind a tunnel: the tunnel's).
    pub remote: String,
}

#[derive(Clone)]
enum Inner {
    Quic(quinn::Connection),
    Mux(Mux),
}

impl Link {
    /// Opens a stream both ways.
    pub async fn open(&self) -> io::Result<(Send, Recv)> {
        match &self.inner {
            Inner::Quic(c) => {
                let (s, r) = c.open_bi().await.map_err(err)?;
                Ok((Box::new(s), Box::new(r)))
            }
            Inner::Mux(m) => m.open().await,
        }
    }

    /// The next stream the peer opened; an error once the link has ended.
    pub async fn accept(&self) -> io::Result<(Send, Recv)> {
        match &self.inner {
            Inner::Quic(c) => {
                let (s, r) = c.accept_bi().await.map_err(err)?;
                Ok((Box::new(s), Box::new(r)))
            }
            Inner::Mux(m) => m.accept().await,
        }
    }

    /// Ends the link.
    pub fn close(&self, reason: &str) {
        match &self.inner {
            Inner::Quic(c) => c.close(0u32.into(), reason.as_bytes()),
            Inner::Mux(m) => m.close(),
        }
    }

    /// Resolves when the link has ended.
    pub async fn closed(&self) {
        match &self.inner {
            Inner::Quic(c) => {
                c.closed().await;
            }
            Inner::Mux(m) => m.closed().await,
        }
    }
}

// ---------------------------------------------------------------------------------------
// QUIC

fn quic_transport() -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    t.keep_alive_interval(Some(Duration::from_secs(5)));
    t.max_idle_timeout(Some(Duration::from_secs(30).try_into().unwrap()));
    t.max_concurrent_bidi_streams(256u32.into());
    Arc::new(t)
}

/// Connects over QUIC to `host:port`.
pub async fn connect_quic(hostport: &str, tls: rustls::ClientConfig) -> io::Result<Link> {
    let addr = tokio::net::lookup_host(hostport)
        .await?
        .next()
        .ok_or_else(|| err(format!("{hostport}: no address")))?;
    let mut cfg = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(tls).map_err(err)?,
    ));
    cfg.transport_config(quic_transport());
    let bind: SocketAddr = if addr.is_ipv6() {
        "[::]:0".parse().unwrap()
    } else {
        "0.0.0.0:0".parse().unwrap()
    };
    let mut ep = quinn::Endpoint::client(bind)?;
    ep.set_default_client_config(cfg);
    let connecting = ep.connect(addr, crate::tls::SERVER_NAME).map_err(err)?;
    let conn = tokio::time::timeout(CONNECT_TIMEOUT, connecting)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, format!("UDP {addr}: no answer")))?
        .map_err(err)?;
    // The endpoint lives as long as the connection.
    let c = conn.clone();
    tokio::spawn(async move {
        c.closed().await;
        ep.wait_idle().await;
    });
    Ok(quic_link(conn))
}

fn quic_link(conn: quinn::Connection) -> Link {
    let alpn = conn
        .handshake_data()
        .and_then(|h| h.downcast::<quinn::crypto::rustls::HandshakeData>().ok())
        .and_then(|h| h.protocol)
        .unwrap_or_default();
    let peer_certs = conn
        .peer_identity()
        .and_then(|p| p.downcast::<Vec<CertificateDer<'static>>>().ok())
        .map(|b| *b);
    Link {
        remote: conn.remote_address().to_string(),
        inner: Inner::Quic(conn),
        alpn,
        peer_certs,
        transport: "quic",
    }
}

/// The Site Manager's QUIC side.
pub struct QuicServer {
    endpoint: quinn::Endpoint,
    /// Links whose handshake finished, from the accepting task (started on first use).
    links: tokio::sync::Mutex<Option<mpsc::Receiver<Link>>>,
}

/// Handshakes at once, in all and from one address: each went in turn, 10 s each, so one
/// slow or silent client held every machine's link up (deep bug run 2026-11-09).
const MAX_HANDSHAKES: usize = 64;
const HANDSHAKES_PER_ADDRESS: usize = 8;

/// A handshake counted until it ends.
struct Handshaking {
    busy: Arc<std::sync::Mutex<(usize, std::collections::HashMap<std::net::IpAddr, usize>)>>,
    ip: std::net::IpAddr,
}

impl Handshaking {
    fn start(
        busy: &Arc<std::sync::Mutex<(usize, std::collections::HashMap<std::net::IpAddr, usize>)>>,
        ip: std::net::IpAddr,
    ) -> Option<Handshaking> {
        let mut b = busy.lock_ok();
        let mine = b.1.get(&ip).copied().unwrap_or(0);
        if b.0 >= MAX_HANDSHAKES || mine >= HANDSHAKES_PER_ADDRESS {
            return None;
        }
        b.0 += 1;
        b.1.insert(ip, mine + 1);
        Some(Handshaking {
            busy: busy.clone(),
            ip,
        })
    }
}

impl Drop for Handshaking {
    fn drop(&mut self) {
        let mut b = self.busy.lock_ok();
        b.0 -= 1;
        if let Some(n) = b.1.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                b.1.remove(&self.ip);
            }
        }
    }
}

impl QuicServer {
    /// Listens on UDP `addr`.
    pub fn bind(addr: SocketAddr, tls: rustls::ServerConfig) -> io::Result<QuicServer> {
        let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(tls).map_err(err)?,
        ));
        cfg.transport_config(quic_transport());
        Ok(QuicServer {
            endpoint: quinn::Endpoint::server(cfg, addr)?,
            links: tokio::sync::Mutex::new(None),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    /// The next link; `None` once closed. Handshakes that fail are skipped (logged); they
    /// run side by side, within [`MAX_HANDSHAKES`] and [`HANDSHAKES_PER_ADDRESS`].
    pub async fn accept(&self) -> Option<Link> {
        let mut links = self.links.lock().await;
        if links.is_none() {
            let (tx, rx) = mpsc::channel(64);
            tokio::spawn(accepting(self.endpoint.clone(), tx));
            *links = Some(rx);
        }
        links.as_mut()?.recv().await
    }

    pub fn close(&self) {
        self.endpoint.close(0u32.into(), b"closed");
    }
}

/// Takes connections on `ep`, each one's handshake on a task of its own, the links made
/// sent on `tx`; ends when the endpoint closes or nobody takes links any more.
async fn accepting(ep: quinn::Endpoint, tx: mpsc::Sender<Link>) {
    let busy = Arc::new(std::sync::Mutex::new((
        0usize,
        std::collections::HashMap::new(),
    )));
    while let Some(incoming) = ep.accept().await {
        if tx.is_closed() {
            return;
        }
        // From the internet: the address proven first (a Retry), so forged sources cost
        // a packet each.
        if !incoming.remote_address_validated() && incoming.may_retry() {
            let _ = incoming.retry();
            continue;
        }
        let Some(counted) = Handshaking::start(&busy, incoming.remote_address().ip()) else {
            incoming.refuse();
            continue;
        };
        let tx = tx.clone();
        tokio::spawn(async move {
            let _counted = counted;
            match tokio::time::timeout(CONNECT_TIMEOUT, async { incoming.await }).await {
                Ok(Ok(c)) => {
                    let _ = tx.send(quic_link(c)).await;
                }
                Ok(Err(e)) => {
                    idn_open_protocol::log::log(&format!("industrial: QUIC handshake: {e}"))
                }
                Err(_) => {}
            }
        });
    }
}

// ---------------------------------------------------------------------------------------
// WebSocket: bytes over binary messages, IDN's TLS over the bytes, yamux over the TLS.

/// A WebSocket's messages as a byte stream both ways.
fn ws_bytes<St, Si>(stream: St, sink: Si) -> impl AsyncRead + AsyncWrite + std::marker::Send + Unpin
where
    St: futures::Stream<Item = io::Result<Bytes>> + std::marker::Send + Unpin + 'static,
    Si: futures::Sink<Bytes, Error = io::Error> + std::marker::Send + Unpin + 'static,
{
    let reader = tokio_util::io::StreamReader::new(stream);
    let writer = tokio_util::io::SinkWriter::new(tokio_util::io::CopyToBytes::new(sink));
    tokio::io::join(reader, writer)
}

/// Binary messages only; a close (or an error) ends the stream; pings are answered by
/// the WebSocket library.
fn tungstenite_bytes<S>(
    ws: tokio_tungstenite::WebSocketStream<S>,
) -> impl AsyncRead + AsyncWrite + std::marker::Send + Unpin
where
    S: AsyncRead + AsyncWrite + std::marker::Send + Unpin + 'static,
{
    use tokio_tungstenite::tungstenite::Message;
    let (sink, stream) = ws.split();
    let stream = stream
        .take_while(|m| futures::future::ready(matches!(m, Ok(m) if !m.is_close())))
        .filter_map(|m| {
            futures::future::ready(match m {
                Ok(Message::Binary(b)) => Some(Ok(b)),
                _ => None,
            })
        });
    let sink = sink
        .sink_map_err(err)
        .with(|b: Bytes| futures::future::ready(Ok::<_, io::Error>(Message::Binary(b))));
    ws_bytes(Box::pin(stream), Box::pin(sink))
}

/// An upgraded WebSocket (axum) as bytes both ways: binary messages only, a close ends
/// it. (MQTT over WebSocket uses it too.)
pub fn axum_bytes(
    ws: axum::extract::ws::WebSocket,
) -> impl AsyncRead + AsyncWrite + std::marker::Send + Unpin {
    use axum::extract::ws::Message;
    let (sink, stream) = ws.split();
    let stream = stream
        .take_while(|m| {
            futures::future::ready(matches!(m, Ok(m) if !matches!(m, Message::Close(_))))
        })
        .filter_map(|m| {
            futures::future::ready(match m {
                Ok(Message::Binary(b)) => Some(Ok(b)),
                _ => None,
            })
        });
    let sink = sink
        .sink_map_err(err)
        .with(|b: Bytes| futures::future::ready(Ok::<_, io::Error>(Message::Binary(b))));
    ws_bytes(Box::pin(stream), Box::pin(sink))
}

/// The Site Manager's side of a WebSocket the HTTP server upgraded (axum).
pub async fn accept_ws(
    ws: axum::extract::ws::WebSocket,
    tls: Arc<rustls::ServerConfig>,
    remote: String,
) -> io::Result<Link> {
    let io = axum_bytes(ws);
    let tls = tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio_rustls::TlsAcceptor::from(tls).accept(io),
    )
    .await
    .map_err(|_| err("TLS inside the WebSocket took too long"))??;
    let (_, conn) = tls.get_ref();
    let alpn = conn.alpn_protocol().map(<[u8]>::to_vec).unwrap_or_default();
    let peer_certs = conn
        .peer_certificates()
        .map(|c| c.iter().map(|c| c.clone().into_owned()).collect());
    Ok(Link {
        inner: Inner::Mux(Mux::start(tls, yamux::Mode::Server)),
        alpn,
        peer_certs,
        transport: "websocket",
        remote,
    })
}

/// The web's certificate authorities, for the WebSocket's outer TLS (to a Cloudflare
/// edge, or any HTTPS front).
fn web_tls() -> io::Result<rustls::ClientConfig> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let mut c =
        rustls::ClientConfig::builder_with_provider(idn_open_protocol::secure::crypto_provider())
            .with_safe_default_protocol_versions()
            .map_err(err)?
            .with_root_certificates(roots)
            .with_no_client_auth();
    c.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(c)
}

/// Connects a WebSocket link to `url` (`wss://host[:port]/idn/link`, or `ws://` on a
/// local network or in tests), then IDN's TLS (`tls`) inside it.
pub async fn connect_ws(url: &str, tls: rustls::ClientConfig) -> io::Result<Link> {
    let (secure, rest) = if let Some(r) = url.strip_prefix("wss://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("ws://") {
        (false, r)
    } else {
        return Err(err(format!("{url}: not a ws:// or wss:// address")));
    };
    let host_port = rest.split('/').next().unwrap_or(rest);
    let host = host_port
        .rsplit_once(':')
        .filter(|(_, p)| p.parse::<u16>().is_ok() && !host_port.ends_with(']'))
        .map_or(host_port, |(h, _)| h)
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let port = host_port
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok())
        .unwrap_or(if secure { 443 } else { 80 });
    let work = async {
        let tcp = tokio::net::TcpStream::connect((host.as_str(), port)).await?;
        let _ = tcp.set_nodelay(true);
        let mut req =
            tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(url)
                .map_err(err)?;
        req.headers_mut()
            .insert("Sec-WebSocket-Protocol", WS_PROTOCOL.parse().map_err(err)?);
        let io: Box<dyn AsyncReadWrite> = if secure {
            let name = ServerName::try_from(host.clone()).map_err(err)?;
            let outer = tokio_rustls::TlsConnector::from(Arc::new(web_tls()?))
                .connect(name, tcp)
                .await?;
            let (ws, _) = tokio_tungstenite::client_async(req, outer)
                .await
                .map_err(err)?;
            Box::new(tungstenite_bytes(ws))
        } else {
            let (ws, _) = tokio_tungstenite::client_async(req, tcp)
                .await
                .map_err(err)?;
            Box::new(tungstenite_bytes(ws))
        };
        let inner = tokio_rustls::TlsConnector::from(Arc::new(tls))
            .connect(ServerName::try_from(crate::tls::SERVER_NAME).unwrap(), io)
            .await?;
        let (_, conn) = inner.get_ref();
        let alpn = conn.alpn_protocol().map(<[u8]>::to_vec).unwrap_or_default();
        let peer_certs = conn
            .peer_certificates()
            .map(|c| c.iter().map(|c| c.clone().into_owned()).collect());
        Ok::<_, io::Error>(Link {
            inner: Inner::Mux(Mux::start(inner, yamux::Mode::Client)),
            alpn,
            peer_certs,
            transport: "websocket",
            remote: format!("{host}:{port}"),
        })
    };
    tokio::time::timeout(CONNECT_TIMEOUT, work)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, format!("{url}: no answer")))?
}

trait AsyncReadWrite: AsyncRead + AsyncWrite + std::marker::Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + std::marker::Send + Unpin> AsyncReadWrite for T {}

/// yamux over a byte stream, driven by a task of its own: opening streams is asked of it
/// over a channel, and streams the peer opens come out of another.
#[derive(Clone)]
struct Mux {
    open: mpsc::Sender<oneshot::Sender<io::Result<yamux::Stream>>>,
    inbound: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<yamux::Stream>>>,
    done: tokio::sync::watch::Receiver<bool>,
    stop: Arc<tokio::sync::Notify>,
}

impl Mux {
    fn start<T>(io: T, mode: yamux::Mode) -> Mux
    where
        T: AsyncRead + AsyncWrite + std::marker::Send + Unpin + 'static,
    {
        let mut cfg = yamux::Config::default();
        cfg.set_max_num_streams(512);
        let mut conn = yamux::Connection::new(io.compat(), cfg, mode);
        let (open, mut open_rx) = mpsc::channel::<oneshot::Sender<io::Result<yamux::Stream>>>(64);
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let (done_tx, done) = tokio::sync::watch::channel(false);
        let stop = Arc::new(tokio::sync::Notify::new());
        let stop2 = stop.clone();
        tokio::spawn(async move {
            let mut pending: Option<oneshot::Sender<io::Result<yamux::Stream>>> = None;
            let mut stopping = std::pin::pin!(stop2.notified());
            let drive = futures::future::poll_fn(|cx| {
                use std::task::Poll;
                if stopping.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(());
                }
                // Streams asked for.
                loop {
                    if pending.is_none() {
                        match open_rx.poll_recv(cx) {
                            Poll::Ready(Some(tx)) => pending = Some(tx),
                            Poll::Ready(None) | Poll::Pending => {}
                        }
                    }
                    let Some(tx) = pending.take() else { break };
                    match conn.poll_new_outbound(cx) {
                        Poll::Ready(r) => {
                            let _ = tx.send(r.map_err(err));
                        }
                        Poll::Pending => {
                            pending = Some(tx);
                            break;
                        }
                    }
                }
                // The peer's streams; this also moves every stream's bytes.
                loop {
                    match conn.poll_next_inbound(cx) {
                        Poll::Ready(Some(Ok(s))) => {
                            let _ = in_tx.send(s);
                        }
                        Poll::Ready(Some(Err(_))) | Poll::Ready(None) => return Poll::Ready(()),
                        Poll::Pending => return Poll::Pending,
                    }
                }
            });
            use std::future::Future;
            drive.await;
            let _ = futures::future::poll_fn(|cx| conn.poll_close(cx)).await;
            let _ = done_tx.send(true);
        });
        Mux {
            open,
            inbound: Arc::new(tokio::sync::Mutex::new(in_rx)),
            done,
            stop,
        }
    }

    fn halves(s: yamux::Stream) -> (Send, Recv) {
        let (r, w) = tokio::io::split(s.compat());
        (Box::new(w), Box::new(r))
    }

    async fn open(&self) -> io::Result<(Send, Recv)> {
        let (tx, rx) = oneshot::channel();
        self.open.send(tx).await.map_err(|_| err("link ended"))?;
        let s = rx.await.map_err(|_| err("link ended"))??;
        Ok(Self::halves(s))
    }

    async fn accept(&self) -> io::Result<(Send, Recv)> {
        let s = self
            .inbound
            .lock()
            .await
            .recv()
            .await
            .ok_or_else(|| err("link ended"))?;
        Ok(Self::halves(s))
    }

    fn close(&self) {
        self.stop.notify_one();
    }

    async fn closed(&self) {
        let mut d = self.done.clone();
        let _ = d.wait_for(|v| *v).await;
    }
}
