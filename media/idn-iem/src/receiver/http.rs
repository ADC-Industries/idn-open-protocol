//! The desk's API from the phone (idn-iem.md §I.8.1): one GET (`/api/v1/iem`) and the
//! push channel (`/api/v1/events`, a WebSocket), over plain HTTP on the desk's own
//! machine or HTTPS on its network port, whose certificate is pinned: the desk's own
//! certificate, the site CA that signed it, or its SHA-256 fingerprint. No web
//! authorities: a desk on a church LAN has none.

use super::{Error, Trust};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, SignatureScheme};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

/// The desk's API port on its own machine (plain HTTP, loopback only).
pub const DESK_PORT: u16 = 29214;
/// The desk's API port on the network (HTTPS; with a site token).
pub const DESK_LAN_PORT: u16 = 29215;

/// The longest answer read (the description is a few KB).
const MAX_ANSWER: usize = 1 << 20;

/// A connection to the desk: plain or TLS.
pub(crate) enum Stream {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Stream {
    pub fn tcp(&self) -> &TcpStream {
        match self {
            Stream::Plain(s) => s,
            Stream::Tls(s) => &s.sock,
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
        }
    }
}

/// Where the desk's API is, and how it is trusted.
#[derive(Clone)]
pub(crate) struct Endpoint {
    /// As given (a name or an address, without brackets), for TLS and `Host:`.
    pub host: String,
    pub port: u16,
    pub addr: SocketAddr,
    pub tls: Option<Arc<rustls::ClientConfig>>,
}

/// `HOST[:PORT]` (`[v6]:PORT` for IPv6) as host and port; `None` for no port.
pub(crate) fn split_host(desk: &str) -> Result<(String, Option<u16>), Error> {
    let desk = desk.trim();
    let bad = || Error::Config(format!("desk {desk:?}: HOST[:PORT]"));
    if let Some(rest) = desk.strip_prefix('[') {
        let (h, after) = rest.split_once(']').ok_or_else(bad)?;
        let port = match after.strip_prefix(':') {
            Some(p) => Some(p.parse().map_err(|_| bad())?),
            None if after.is_empty() => None,
            None => return Err(bad()),
        };
        return Ok((h.to_owned(), port));
    }
    match desk.rsplit_once(':') {
        // (A bare IPv6 address has several colons: no port.)
        Some((h, p)) if !h.contains(':') => Ok((h.to_owned(), Some(p.parse().map_err(|_| bad())?))),
        _ if desk.is_empty() => Err(bad()),
        _ => Ok((desk.to_owned(), None)),
    }
}

impl Endpoint {
    /// The desk at `desk` (`HOST[:PORT]`; no port: 29214 plain, 29215 with `trust`).
    pub fn new(desk: &str, trust: Option<&Trust>) -> Result<Endpoint, Error> {
        let (host, port) = split_host(desk)?;
        let port = port.unwrap_or(if trust.is_some() {
            DESK_LAN_PORT
        } else {
            DESK_PORT
        });
        let all: Vec<SocketAddr> = (host.as_str(), port)
            .to_socket_addrs()
            .map_err(|e| Error::Io(format!("{host}: {e}")))?
            .collect();
        // IPv4 first: the multicast group is IPv4.
        let addr = all
            .iter()
            .find(|a| a.is_ipv4())
            .or(all.first())
            .copied()
            .ok_or_else(|| Error::Io(format!("{host}: no address")))?;
        let tls = trust.map(client_config).transpose()?;
        Ok(Endpoint {
            host,
            port,
            addr,
            tls,
        })
    }

    fn host_header(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// A connection (TLS done when trusted), reads waiting at most `timeout`.
    pub fn connect(&self, timeout: Duration) -> Result<Stream, Error> {
        let tcp = TcpStream::connect_timeout(&self.addr, timeout)
            .map_err(|e| Error::Io(format!("{}: {e}", self.addr)))?;
        let _ = tcp.set_nodelay(true);
        let _ = tcp.set_read_timeout(Some(timeout));
        let _ = tcp.set_write_timeout(Some(timeout));
        let Some(cfg) = &self.tls else {
            return Ok(Stream::Plain(tcp));
        };
        let name = ServerName::try_from(self.host.clone())
            .map_err(|e| Error::Config(format!("{}: {e}", self.host)))?;
        let conn = rustls::ClientConnection::new(cfg.clone(), name)
            .map_err(|e| Error::Tls(e.to_string()))?;
        let mut s = rustls::StreamOwned::new(conn, tcp);
        // The handshake now, so a certificate refused is said as such.
        while s.conn.is_handshaking() {
            s.conn.complete_io(&mut s.sock).map_err(|e| {
                match e.get_ref().and_then(|i| i.downcast_ref::<rustls::Error>()) {
                    Some(t) => Error::Tls(t.to_string()),
                    None if e.kind() == io::ErrorKind::InvalidData => Error::Tls(e.to_string()),
                    None => Error::Io(e.to_string()),
                }
            })?;
        }
        Ok(Stream::Tls(Box::new(s)))
    }

    /// `GET path` with `token`: the status and the body.
    pub fn get(&self, path: &str, token: &str) -> Result<(u16, Vec<u8>), Error> {
        let mut s = self.connect(Duration::from_secs(5))?;
        let mut req = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\nAccept: application/json\r\nConnection: close\r\n",
            self.host_header()
        );
        if !token.is_empty() {
            req.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        req.push_str("\r\n");
        s.write_all(req.as_bytes())
            .and_then(|_| s.flush())
            .map_err(|e| Error::Io(e.to_string()))?;
        read_answer(&mut s)
    }

    /// The push channel at `path` with `token` (`Authorization: Bearer`).
    pub fn websocket(
        &self,
        path: &str,
        token: &str,
    ) -> Result<tungstenite::WebSocket<Stream>, Error> {
        use tungstenite::client::IntoClientRequest;
        let s = self.connect(Duration::from_secs(5))?;
        let scheme = if self.tls.is_some() { "wss" } else { "ws" };
        let mut req = format!("{scheme}://{}{path}", self.host_header())
            .into_client_request()
            .map_err(|e| Error::Config(e.to_string()))?;
        if !token.is_empty() {
            let v = tungstenite::http::HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| Error::Config("the token: not a header's text".into()))?;
            req.headers_mut().insert("Authorization", v);
        }
        match tungstenite::client::client(req, s) {
            Ok((ws, _)) => Ok(ws),
            Err(tungstenite::HandshakeError::Failure(tungstenite::Error::Http(r))) => {
                let body = r.body().clone().unwrap_or_default();
                Err(status_error(r.status().as_u16(), &body, token))
            }
            Err(tungstenite::HandshakeError::Failure(e)) => Err(Error::Io(e.to_string())),
            Err(tungstenite::HandshakeError::Interrupted(_)) => Err(Error::Timeout),
        }
    }
}

/// An HTTP answer's error status as an error: 401/403 the token's, others as they are.
pub(crate) fn status_error(status: u16, body: &[u8], token: &str) -> Error {
    let why = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["error"].as_str().map(str::to_owned))
        .unwrap_or_else(|| {
            String::from_utf8_lossy(body)
                .trim()
                .chars()
                .take(200)
                .collect()
        });
    match status {
        401 | 403 if token.is_empty() => Error::TokenNeeded(why),
        401 => Error::TokenRefused(why),
        403 => Error::NotAllowed(why),
        s => Error::Http(s, why),
    }
}

/// An answer: its status and its body (by `Content-Length`, chunked, or to the end).
fn read_answer(s: &mut Stream) -> Result<(u16, Vec<u8>), Error> {
    let mut buf = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    let bad = |m: &str| Error::Http(0, format!("a bad answer: {m}"));
    let mut head_end = None;
    let mut want: Option<usize> = None;
    let mut chunked = false;
    let mut status = 0u16;
    loop {
        if let Some(h) = head_end {
            let body = &buf[h..];
            if want.is_some_and(|n| body.len() >= n) || (chunked && dechunk(body).is_some()) {
                break;
            }
        }
        let n = match s.read(&mut chunk) {
            Ok(n) => n,
            // (A TLS peer that closes without close_notify: the end.)
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => 0,
            Err(e) => return Err(Error::Io(e.to_string())),
        };
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_ANSWER {
            return Err(bad("too long"));
        }
        if head_end.is_none()
            && let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n")
        {
            head_end = Some(p + 4);
            let head = String::from_utf8_lossy(&buf[..p]).into_owned();
            let mut lines = head.split("\r\n");
            status = lines
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|c| c.parse().ok())
                .ok_or_else(|| bad("no status"))?;
            for l in lines {
                let Some((k, v)) = l.split_once(':') else {
                    continue;
                };
                let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
                if k == "content-length" {
                    want = v.parse().ok();
                } else if k == "transfer-encoding" && v.eq_ignore_ascii_case("chunked") {
                    chunked = true;
                }
            }
        }
    }
    let h = head_end.ok_or_else(|| bad("no header"))?;
    let body = &buf[h..];
    let body = if chunked {
        dechunk(body).ok_or_else(|| bad("chunks cut short"))?
    } else if let Some(n) = want {
        body.get(..n).ok_or_else(|| bad("body cut short"))?.to_vec()
    } else {
        body.to_vec()
    };
    Ok((status, body))
}

/// A chunked body joined, once its last chunk is there.
fn dechunk(mut b: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let p = b.windows(2).position(|w| w == b"\r\n")?;
        let size = std::str::from_utf8(&b[..p]).ok()?;
        let size = usize::from_str_radix(size.split(';').next()?.trim(), 16).ok()?;
        b = &b[p + 2..];
        if size == 0 {
            return Some(out);
        }
        // (A size past any answer: refused, not overflowed into a panic.)
        if size > MAX_ANSWER || b.len() < size + 2 {
            return None;
        }
        out.extend_from_slice(&b[..size]);
        b = &b[size + 2..];
    }
}

/// The TLS setup for `trust`.
fn client_config(trust: &Trust) -> Result<Arc<rustls::ClientConfig>, Error> {
    // (ring's own, as `idn_open_protocol::secure::crypto_provider` is: this crate depends
    // on nothing of IDN's, for the phone builds.)
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Pinned::new(trust, provider.clone())?;
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Tls(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok(Arc::new(cfg))
}

/// A server certificate is trusted when it is the pinned one (or has the pinned
/// fingerprint), or when the pinned certificate (the site CA) signed it. The name isn't
/// checked against a CA's certificate: phones reach desks by address, and every machine
/// the site CA signed is the site's.
#[derive(Debug)]
struct Pinned {
    cert: Option<Vec<u8>>,
    sha256: Option<[u8; 32]>,
    chain: Option<Arc<rustls::client::WebPkiServerVerifier>>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl Pinned {
    fn new(trust: &Trust, provider: Arc<rustls::crypto::CryptoProvider>) -> Result<Pinned, Error> {
        match trust {
            Trust::Certificate(der) => {
                let mut roots = rustls::RootCertStore::empty();
                // (A desk's own self-signed certificate isn't a CA: equality alone.)
                let chain = roots
                    .add(CertificateDer::from(der.clone()))
                    .ok()
                    .and_then(|_| {
                        rustls::client::WebPkiServerVerifier::builder_with_provider(
                            Arc::new(roots),
                            provider.clone(),
                        )
                        .build()
                        .ok()
                    });
                Ok(Pinned {
                    cert: Some(der.clone()),
                    sha256: None,
                    chain,
                    provider,
                })
            }
            Trust::Sha256(fp) => {
                let hex: String = fp.chars().filter(|c| c.is_ascii_hexdigit()).collect();
                let bytes: Vec<u8> = (0..hex.len() / 2)
                    .filter_map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok())
                    .collect();
                let sha256: [u8; 32] = bytes.try_into().map_err(|_| {
                    Error::Config("fingerprint: SHA-256, 64 hex digits (colons allowed)".into())
                })?;
                Ok(Pinned {
                    cert: None,
                    sha256: Some(sha256),
                    chain: None,
                    provider,
                })
            }
        }
    }
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if self
            .cert
            .as_ref()
            .is_some_and(|c| c.as_slice() == end_entity.as_ref())
        {
            return Ok(ServerCertVerified::assertion());
        }
        if let Some(fp) = &self.sha256
            && ring::digest::digest(&ring::digest::SHA256, end_entity.as_ref()).as_ref() == fp
        {
            return Ok(ServerCertVerified::assertion());
        }
        if let Some(v) = &self.chain {
            return match v.verify_server_cert(
                end_entity,
                intermediates,
                server_name,
                ocsp_response,
                now,
            ) {
                Ok(ok) => Ok(ok),
                Err(rustls::Error::InvalidCertificate(
                    CertificateError::NotValidForName
                    | CertificateError::NotValidForNameContext { .. },
                )) => Ok(ServerCertVerified::assertion()),
                Err(e) => Err(e),
            };
        }
        Err(rustls::Error::InvalidCertificate(
            CertificateError::UnknownIssuer,
        ))
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_and_ports() {
        assert_eq!(
            split_host("desk.local").unwrap(),
            ("desk.local".into(), None)
        );
        assert_eq!(
            split_host("10.0.0.5:29215").unwrap(),
            ("10.0.0.5".into(), Some(29215))
        );
        assert_eq!(
            split_host("[fe80::1]:80").unwrap(),
            ("fe80::1".into(), Some(80))
        );
        assert_eq!(split_host("fe80::1").unwrap(), ("fe80::1".into(), None));
        assert!(split_host("a:b").is_err());
        assert!(split_host("").is_err());
    }

    #[test]
    fn chunked_bodies() {
        assert_eq!(
            dechunk(b"3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n").unwrap(),
            b"abcde"
        );
        assert!(dechunk(b"3\r\nabc\r\n").is_none());
    }

    #[test]
    fn statuses_name_the_token() {
        assert!(matches!(
            status_error(401, b"{}", ""),
            Error::TokenNeeded(_)
        ));
        assert!(matches!(
            status_error(401, br#"{"error":"expired"}"#, "t"),
            Error::TokenRefused(m) if m == "expired"
        ));
        assert!(matches!(status_error(403, b"", "t"), Error::NotAllowed(_)));
        assert!(matches!(status_error(404, b"", "t"), Error::Http(404, _)));
    }
}
