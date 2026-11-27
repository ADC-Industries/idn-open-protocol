//! TLS on the control connection: the site-key verifier, the handshake, the plaintext pipe.

use super::*;

/// Accepts exactly the site certificate's public key (§E.8.1), in both directions.
#[derive(Debug)]
pub(super) struct SiteVerifier {
    key: Arc<SiteKey>,
    algs: WebPkiSupportedAlgorithms,
}

impl SiteVerifier {
    pub(super) fn new(key: Arc<SiteKey>, p: &CryptoProvider) -> Self {
        SiteVerifier {
            key,
            algs: p.signature_verification_algorithms,
        }
    }

    pub(super) fn check(&self, cert: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        match ed25519_public_key(cert) {
            Some(k) if k == self.key.public_key => Ok(()),
            _ => Err(rustls::Error::General(
                "peer does not hold the site key".into(),
            )),
        }
    }
}

impl ServerCertVerifier for SiteVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|_| ServerCertVerified::assertion())
    }
    crate::secure::ed25519_tls13_signatures!(algs);
}

impl ClientCertVerifier for SiteVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|_| ClientCertVerified::assertion())
    }
    crate::secure::ed25519_tls13_signatures!(algs);
}

/// Runs the TLS handshake on `net`, then returns the plaintext end of a loopback socket
/// pair; two threads move data between it and the encrypted connection.
#[doc(hidden)]
pub fn wrap(net: TcpStream, conn: rustls::Connection) -> io::Result<TcpStream> {
    wrap_with_peer(net, conn).map(|(s, _)| s)
}

/// A connected loopback TCP pair carrying decrypted data. Only our own connection is
/// accepted: another local process that connects to the listener in the same moment
/// must not get the plaintext. Our connection is already queued when `accept` runs, so
/// any others ahead of it are dropped and the loop ends quickly.
pub(crate) fn loopback_pair() -> io::Result<(TcpStream, TcpStream)> {
    loopback_pair_with(|_| {})
}

/// [`loopback_pair`], with `before` run between bind and connect (for tests: a process
/// that gets in first).
pub(super) fn loopback_pair_with(
    before: impl FnOnce(std::net::SocketAddr),
) -> io::Result<(TcpStream, TcpStream)> {
    let l = TcpListener::bind(("127.0.0.1", 0))?;
    before(l.local_addr()?);
    let app = TcpStream::connect(l.local_addr()?)?;
    let ours = app.local_addr()?;
    loop {
        let (pipe, from) = l.accept()?;
        if from == ours {
            return Ok((app, pipe));
        }
    }
}

/// The longest a TLS handshake may take.
const HANDSHAKE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// As [`wrap`], also returning the peer's end-entity certificate (DER), if it sent one.
#[doc(hidden)]
pub fn wrap_with_peer(
    mut net: TcpStream,
    mut conn: rustls::Connection,
) -> io::Result<(TcpStream, Option<Vec<u8>>)> {
    // An overall deadline, not only one per read: a peer trickling a byte every few
    // seconds must not hold this thread.
    let deadline = std::time::Instant::now() + HANDSHAKE_DEADLINE;
    net.set_write_timeout(Some(crate::transport::WRITE_TIMEOUT))?;
    // One read at a time (complete_io keeps reading for as long as the peer trickles).
    while conn.is_handshaking() {
        while conn.wants_write() {
            conn.write_tls(&mut net)?;
        }
        if !conn.is_handshaking() {
            break;
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "TLS handshake took too long",
            ));
        }
        net.set_read_timeout(Some(left.min(std::time::Duration::from_secs(5))))?;
        if conn.read_tls(&mut net)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if let Err(e) = conn.process_new_packets() {
            // Tell the peer why (the alert), then fail.
            while conn.wants_write() && conn.write_tls(&mut net).is_ok() {}
            return Err(io::Error::new(io::ErrorKind::InvalidData, e));
        }
    }
    while conn.wants_write() {
        conn.write_tls(&mut net)?; // our last handshake flight
    }
    net.set_read_timeout(None)?;
    // The write timeout stays: a peer that stops reading would otherwise hold the thread
    // copying to it (and its socket) for as long as it keeps the connection open; the
    // session's own timeout is on the plaintext side and never reaches this socket.
    net.set_write_timeout(Some(crate::transport::WRITE_TIMEOUT))?;
    let peer_cert = conn
        .peer_certificates()
        .and_then(|c| c.first())
        .map(|c| c.as_ref().to_vec());
    // A connected loopback pair: `app` goes back to the caller, `pipe` is ours.
    let (app, pipe) = loopback_pair()?;
    let _ = app.set_nodelay(true);
    let _ = pipe.set_nodelay(true);
    let conn = Arc::new(Mutex::new(conn));
    let net_w = Arc::new(Mutex::new(net.try_clone()?));
    // Encrypted → plaintext.
    {
        let (conn, net_w, mut net_r, mut pipe_w) = (
            conn.clone(),
            net_w.clone(),
            net.try_clone()?,
            pipe.try_clone()?,
        );
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            let mut plain = Vec::with_capacity(64 * 1024);
            let mut out = Vec::new();
            loop {
                let n = match net_r.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                // The peer sent close_notify: what follows is not part of the session.
                let mut peer_closed = false;
                {
                    let mut c = conn.lock_ok();
                    let mut rd = &buf[..n];
                    plain.clear();
                    let mut tmp = [0u8; 16 * 1024];
                    // Feed ciphertext in pieces and drain plaintext after each one: rustls
                    // refuses more input while its plaintext buffer is full, and the rest of
                    // `rd` must not be lost.
                    while !rd.is_empty() && !peer_closed {
                        let fed = c.read_tls(&mut rd);
                        if let Err(e) = c.process_new_packets() {
                            let _ = c.write_tls(&mut out);
                            drop(c);
                            let _ = net_w.lock_ok().write_all(&out);
                            // The peer's alert after our side of the handshake was done
                            // (TLS 1.3: the client finishes before the server has checked
                            // its certificate): said as the refusal it is (a
                            // receiver revoked while offline saw only an end of file).
                            if let rustls::Error::AlertReceived(alert) = e {
                                let reason = format!(
                                    "the other side refused this device's certificate ({alert:?}): it may have been revoked by the site authority, or belong to another site"
                                );
                                let e = crate::xml::Element::new("x_idn_refused")
                                    .with_attr("reason", &reason);
                                let _ = pipe_w.write_all(
                                    &crate::frame::Frame::text(
                                        crate::frame::kind::CONTROL,
                                        0,
                                        &e.to_xml(),
                                    )
                                    .encode(),
                                );
                            }
                            let _ = pipe_w.shutdown(Shutdown::Both);
                            return;
                        }
                        let drained = plain.len();
                        loop {
                            match c.reader().read(&mut tmp) {
                                Ok(0) => {
                                    peer_closed = true;
                                    break;
                                }
                                Ok(k) => plain.extend_from_slice(&tmp[..k]),
                                Err(_) => break, // WouldBlock: no more plaintext for now
                            }
                        }
                        // Stop when no progress is possible: nothing taken (after
                        // close_notify rustls takes no more), or refused while there was
                        // no plaintext to make room by draining. Offering the same bytes
                        // again would spin forever.
                        match fed {
                            Ok(0) => break,
                            Err(_) if plain.len() == drained => break,
                            _ => {}
                        }
                    }
                    out.clear();
                    while c.wants_write() {
                        if c.write_tls(&mut out).is_err() {
                            break;
                        }
                    }
                }
                if !out.is_empty() && net_w.lock_ok().write_all(&out).is_err() {
                    break;
                }
                if !plain.is_empty() && pipe_w.write_all(&plain).is_err() {
                    break;
                }
                if peer_closed {
                    break;
                }
            }
            let _ = pipe_w.shutdown(Shutdown::Both);
        });
    }
    // Plaintext → encrypted.
    {
        let mut pipe_r = pipe;
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            let mut out = Vec::new();
            loop {
                let n = pipe_r.read(&mut buf).unwrap_or(0);
                out.clear();
                {
                    let mut c = conn.lock_ok();
                    if n == 0 {
                        c.send_close_notify();
                    } else if c.writer().write_all(&buf[..n]).is_err() {
                        break;
                    }
                    while c.wants_write() {
                        if c.write_tls(&mut out).is_err() {
                            break;
                        }
                    }
                }
                let mut w = net_w.lock_ok();
                if w.write_all(&out).is_err() || n == 0 {
                    let _ = w.shutdown(Shutdown::Both);
                    break;
                }
            }
        });
    }
    Ok((app, peer_cert))
}

/// TLS client side (receiver) of the control connection.
#[doc(hidden)]
pub fn connect_tls(key: &Arc<SiteKey>, net: TcpStream) -> io::Result<TcpStream> {
    let name = ServerName::try_from("idn-site").map_err(io::Error::other)?;
    let conn = rustls::ClientConnection::new(Arc::new(key.client_config()?), name)
        .map_err(io::Error::other)?;
    wrap(net, rustls::Connection::Client(conn))
}

/// TLS server side (sender) of the control connection.
#[doc(hidden)]
pub fn accept_tls(key: &Arc<SiteKey>, net: TcpStream) -> io::Result<TcpStream> {
    let conn =
        rustls::ServerConnection::new(Arc::new(key.server_config()?)).map_err(io::Error::other)?;
    wrap(net, rustls::Connection::Server(conn))
}
