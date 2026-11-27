//! Services over the link for clients that are not IDN devices (an IDN Voice handset's
//! app, idn-voice.md §V.14): mode `open` only, encrypted with this machine's link key,
//! whose certificate the client pins by its SHA-256. The client presents no certificate;
//! it proves who it is inside the connection. Connection migration on, so a phone that
//! moves between Wi-Fi and cellular keeps its connection.
//!
//! Reached from the internet: a connection is counted from its first packet until the
//! service has its client signed in ([`ServicePending`]), at most 64 at once in all and 8
//! from one address, and what a client may send before the service reads it is the
//! service's to bound ([`ServiceTransport`]).

use super::*;

/// A service's QUIC settings, read for each connection (a changed setting counts from the
/// next one).
#[derive(Debug, Clone, Copy)]
pub struct ServiceTransport {
    /// QUIC keep-alive: a NAT's mapping held, a dead path noticed.
    pub keep_alive: Duration,
    /// No packet for this long: the connection is gone.
    pub idle_timeout: Duration,
    /// Streams both ways the client may have open at once (no one-way streams).
    pub streams: u32,
    /// Bytes the client may send on a stream ahead of the service's reading.
    pub stream_window: u32,
    /// ... and on the whole connection (quinn's default is unbounded).
    pub receive_window: u32,
}

/// A connection whose client hasn't signed in yet: counted, in all and per address with
/// the handshakes, until the service drops this, once the client has proved who it is
/// (or the connection has ended). Before, the count ended with the handshake, and any
/// number of connections could sit out their sign-in time.
pub struct ServicePending {
    _counted: SiteHandshake,
}

/// Listens on UDP for the services' ALPNs; each connection made goes to its handler.
pub struct ServiceListener {
    endpoint: quinn::Endpoint,
    port: u16,
    cert_sha256: String,
}

/// What a service may hold unacknowledged: a slow client's backlog stays in the service's
/// own queue (which drops what is too old), not in QUIC's.
const SERVICE_SEND_WINDOW: u64 = 256 << 10;

impl ServiceListener {
    /// Listens on UDP `addr` (port 0: any) for `alpns`, presenting `link`'s key; each
    /// connection made goes to `on_accept` with its ALPN and its [`ServicePending`] (drop
    /// it once the client is signed in), on a runtime thread (hand it over). `transport`
    /// is asked for each connection's settings.
    pub fn bind(
        addr: SocketAddr,
        link: &LinkConfig,
        alpns: &[&str],
        transport: impl Fn() -> ServiceTransport + Send + Sync + 'static,
        on_accept: impl Fn(&str, quinn::Connection, ServicePending) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let sock = UdpSocket::bind(addr)?;
        let port = sock.local_addr()?.port();
        let identity = link.identity.clone();
        let provider = crate::secure::crypto_provider();
        let mut tls = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(io::Error::other)?
            .with_no_client_auth()
            .with_cert_resolver(identity.certified(&provider)?);
        tls.alpn_protocols = alpns.iter().map(|a| a.as_bytes().to_vec()).collect();
        // (No 0-RTT: a replayed first frame could repeat a command.)
        tls.max_early_data_size = 0;
        tls.send_tls13_tickets = 0;
        let crypto: Arc<quinn::crypto::rustls::QuicServerConfig> = Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(tls).map_err(io::Error::other)?,
        );
        let config = move |t: ServiceTransport| -> io::Result<quinn::ServerConfig> {
            let mut c = quinn::ServerConfig::with_crypto(crypto.clone());
            c.transport_config(service_transport(t)?);
            c.migration(true);
            Ok(c)
        };
        let first = config(transport())?;
        let endpoint = {
            let _g = crate::quic::runtime().enter();
            quinn::Endpoint::new(
                quinn::EndpointConfig::default(),
                Some(first),
                sock,
                Arc::new(quinn::TokioRuntime),
            )?
        };
        let ep = endpoint.clone();
        let alpns: Vec<String> = alpns.iter().map(|a| (*a).to_owned()).collect();
        let on_accept = Arc::new(on_accept);
        let busy = Arc::new(AtomicUsize::new(0));
        let per_addr: Arc<Mutex<BTreeMap<std::net::IpAddr, usize>>> = Arc::default();
        crate::quic::runtime().spawn(async move {
            // (Ends once the listener is dropped: the endpoint closed.)
            while let Some(incoming) = ep.accept().await {
                // Reached from the internet, as the site listener is: the address proven
                // first, then a cap on connections not yet signed in (handshakes, then
                // the service's sign-in) in all and per address.
                if !incoming.remote_address_validated() && incoming.may_retry() {
                    let _ = incoming.retry();
                    continue;
                }
                let Some(handshake) =
                    SiteHandshake::start(&busy, &per_addr, incoming.remote_address().ip())
                else {
                    incoming.refuse();
                    continue;
                };
                let Ok(cfg) = config(transport()) else {
                    incoming.refuse();
                    continue;
                };
                let (on_accept, alpns) = (on_accept.clone(), alpns.clone());
                tokio::spawn(async move {
                    let peer = incoming.remote_address();
                    let r = async {
                        let connecting = incoming
                            .accept_with(Arc::new(cfg))
                            .map_err(io::Error::other)?;
                        tokio::time::timeout(HANDSHAKE_TIMEOUT, connecting)
                            .await
                            .map_err(|_| io::Error::other("handshake took too long"))?
                            .map_err(io::Error::other)
                    }
                    .await;
                    let conn = match r {
                        Ok(c) => c,
                        Err(e) => {
                            crate::log::log(&format!("link service from {peer}: {e}"));
                            return;
                        }
                    };
                    // (A client may offer no ALPN at all; rustls then picks none.)
                    let alpn = conn
                        .handshake_data()
                        .and_then(|d| d.downcast::<quinn::crypto::rustls::HandshakeData>().ok())
                        .and_then(|d| d.protocol)
                        .and_then(|p| String::from_utf8(p).ok());
                    match alpn.filter(|a| alpns.contains(a)) {
                        Some(a) => on_accept(
                            &a,
                            conn,
                            ServicePending {
                                _counted: handshake,
                            },
                        ),
                        None => conn.close(code::REFUSED.into(), b"no service named"),
                    }
                });
            }
        });
        Ok(ServiceListener {
            endpoint,
            port,
            cert_sha256: crate::hash::sha256_hex(identity.cert_der()),
        })
    }

    /// The UDP port listened on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The SHA-256 of the certificate it presents (64 hex digits): what clients pin.
    pub fn cert_sha256(&self) -> &str {
        &self.cert_sha256
    }
}

impl Drop for ServiceListener {
    fn drop(&mut self) {
        self.endpoint.close(code::GOING_AWAY.into(), b"closed");
    }
}

fn service_transport(t: ServiceTransport) -> io::Result<Arc<quinn::TransportConfig>> {
    let mut c = quinn::TransportConfig::default();
    c.keep_alive_interval(Some(t.keep_alive));
    c.max_idle_timeout(Some(t.idle_timeout.try_into().map_err(io::Error::other)?));
    c.send_window(SERVICE_SEND_WINDOW);
    // The service's streams, and what its client may send ahead of it (quinn's defaults
    // let an unauthenticated client make the server hold megabytes); no datagrams
    // (reliable only).
    c.max_concurrent_bidi_streams(t.streams.into());
    c.max_concurrent_uni_streams(0u32.into());
    c.stream_receive_window(t.stream_window.into());
    c.receive_window(t.receive_window.into());
    c.datagram_receive_buffer_size(None);
    Ok(Arc::new(c))
}
