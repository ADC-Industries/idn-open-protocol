//! A small HTTP/1.1 client (feature `http-client`): one request per connection, TLS
//! through rustls with the web's authorities (or, for a plant device with a self-signed
//! certificate, `insecure`), answers up to 16 MiB, chunked or not. The Industrial edition's
//! polling (§I.10, `http`) and the switcher's data sources (titles from a sheet, a feed)
//! use it.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

const MAX_BODY: usize = 16 << 20;

fn bad(m: impl std::fmt::Display) -> io::Error {
    idn_common::err::invalid_data(format!("http: {m}"))
}

/// (status, body).
pub fn request(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    insecure: bool,
    timeout: Duration,
) -> io::Result<(u16, Vec<u8>)> {
    exchange(method, url, headers, body, insecure, timeout, MAX_BODY).map(|(s, _, b)| (s, b))
}

/// GETs `url`, following redirects (up to 5): the body of a 2xx answer, else an error
/// naming the status.
pub fn get(url: &str, timeout: Duration) -> io::Result<Vec<u8>> {
    get_up_to(url, timeout, MAX_BODY)
}

/// As [`get`], for answers up to `limit` bytes (a program to download: IDN Data's
/// yt-dlp, about 35 MB). `timeout` is for each read, not the whole answer.
pub fn get_up_to(url: &str, timeout: Duration, limit: usize) -> io::Result<Vec<u8>> {
    let mut url = url.to_owned();
    for _ in 0..6 {
        let (status, location, body) = exchange("GET", &url, &[], None, false, timeout, limit)?;
        match status {
            200..=299 => return Ok(body),
            301 | 302 | 303 | 307 | 308 => {
                let to = location.ok_or_else(|| bad("a redirect without a Location"))?;
                url = resolve(&url, &to);
            }
            s => return Err(bad(format!("{s} from {url}"))),
        }
    }
    Err(bad("too many redirects"))
}

/// Where `url` redirects to (one step), or `None` when it answers itself: e.g. which
/// release a project's `releases/latest` is, so its files are all fetched from that one
/// (bug round F, F2-2).
pub fn redirect_of(url: &str, timeout: Duration) -> io::Result<Option<String>> {
    let (status, location, _) = exchange("GET", url, &[], None, false, timeout, MAX_BODY)?;
    match status {
        301 | 302 | 303 | 307 | 308 => {
            let to = location.ok_or_else(|| bad("a redirect without a Location"))?;
            Ok(Some(resolve(url, &to)))
        }
        200..=299 => Ok(None),
        s => Err(bad(format!("{s} from {url}"))),
    }
}

/// What a download must be: its exact size and SHA-256. Anything else is refused and
/// never renamed into place.
#[derive(Clone, Copy, Debug)]
pub struct Expected {
    pub size: u64,
    pub sha256: [u8; 32],
}

/// A connection whose reads go on through short timeouts until the whole `timeout` has
/// passed with nothing, or `stop` is set (then `Interrupted`).
/// (A stop is told as `Other` "stopped" here, made `Interrupted` by
/// [`download_stoppable`]: `Interrupted` from a transport means "read again" to TLS, which
/// would go round for good; bug run B. The time with nothing counts across reads.)
struct Patient<'a> {
    inner: TcpStream,
    stop: Option<&'a std::sync::atomic::AtomicBool>,
    timeout: Duration,
    heard: std::time::Instant,
}

/// The error text a stop is told by ([`Patient`]).
const STOPPED: &str = "stopped";

impl Read for Patient<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.inner.read(buf) {
                Err(e)
                    if self.stop.is_some()
                        && matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                        ) =>
                {
                    if self
                        .stop
                        .is_some_and(|s| s.load(std::sync::atomic::Ordering::Relaxed))
                    {
                        return Err(io::Error::other(STOPPED));
                    }
                    if self.heard.elapsed() >= self.timeout {
                        return Err(e);
                    }
                }
                Ok(n) => {
                    self.heard = std::time::Instant::now();
                    return Ok(n);
                }
                r => return r,
            }
        }
    }
}

impl Write for Patient<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// GETs `url` (following redirects, up to 5) into the file `to`, without holding it in
/// memory (a speech model of a gigabyte or more): written to a part file of its own beside `to` and renamed when
/// whole (and, with `expect`, only when its size and SHA-256 match: hashed as it is
/// written). `progress` gets (bytes so far, the size when known). The bytes written.
pub fn download(
    url: &str,
    to: &std::path::Path,
    timeout: Duration,
    expect: Option<Expected>,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> io::Result<u64> {
    download_stoppable(url, to, timeout, expect, progress, None)
}

/// [`download`], ended when `stop` is set: within about a second, with
/// `ErrorKind::Interrupted`, its part file removed (nothing under `to`). Two downloads to
/// the same `to` at once are safe: each writes a part file of its own, and only a whole,
/// checked file is renamed into place.
pub fn download_stoppable(
    url: &str,
    to: &std::path::Path,
    timeout: Duration,
    expect: Option<Expected>,
    progress: &mut dyn FnMut(u64, Option<u64>),
    stop: Option<&std::sync::atomic::AtomicBool>,
) -> io::Result<u64> {
    // Any error once stopped (the reader's "stopped", or what it cut short) is told as
    // `Interrupted`.
    download_inner(url, to, timeout, expect, progress, stop).map_err(|e| {
        if stop.is_some_and(|s| s.load(std::sync::atomic::Ordering::Relaxed)) {
            io::Error::new(io::ErrorKind::Interrupted, STOPPED)
        } else {
            e
        }
    })
}

fn download_inner(
    url: &str,
    to: &std::path::Path,
    timeout: Duration,
    expect: Option<Expected>,
    progress: &mut dyn FnMut(u64, Option<u64>),
    stop: Option<&std::sync::atomic::AtomicBool>,
) -> io::Result<u64> {
    use sha2::Digest;
    let stopped = || stop.is_some_and(|s| s.load(std::sync::atomic::Ordering::Relaxed));
    let mut url = url.to_owned();
    for _ in 0..6 {
        let (tls, host, port, path, hostport) = split_url(&url)?;
        let addr = (host.as_str(), port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| bad(format!("{host}: no address")))?;
        if stopped() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "stopped"));
        }
        // Stoppable: connecting a second at a time, the stop looked at between tries (a
        // host dropping the connection held a stop up to the whole timeout; bug run B).
        let tcp = if stop.is_some() {
            let since = std::time::Instant::now();
            loop {
                match TcpStream::connect_timeout(&addr, Duration::from_secs(1).min(timeout)) {
                    Ok(t) => break t,
                    Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                        if stopped() {
                            return Err(io::Error::other(STOPPED));
                        }
                        if since.elapsed() >= timeout {
                            return Err(e);
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
        } else {
            TcpStream::connect_timeout(&addr, timeout)?
        };
        // Stoppable: reads of a second at a time, the stop looked at between them
        // ([`Patient`]), the timeout counted across them.
        tcp.set_read_timeout(Some(if stop.is_some() {
            Duration::from_secs(1).min(timeout)
        } else {
            timeout
        }))?;
        tcp.set_write_timeout(Some(timeout))?;
        let tcp = Patient {
            inner: tcp,
            stop,
            timeout,
            heard: std::time::Instant::now(),
        };
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: {hostport}\r\nUser-Agent: idn-open-protocol\r\nConnection: close\r\n\r\n"
        );
        let mut s: Box<dyn Read + '_> = if tls {
            let name = rustls::pki_types::ServerName::try_from(host.clone()).map_err(bad)?;
            let conn = rustls::ClientConnection::new(Arc::new(client_config(false)?), name)
                .map_err(bad)?;
            let mut st = rustls::StreamOwned::new(conn, tcp);
            st.write_all(req.as_bytes())?;
            Box::new(st)
        } else {
            let mut st = tcp;
            st.write_all(req.as_bytes())?;
            Box::new(st)
        };
        // The head, a byte at a time up to its end (it is small).
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if s.read(&mut b)? == 0 || head.len() > 65536 {
                return Err(bad("no answer"));
            }
            head.push(b[0]);
        }
        let head = String::from_utf8_lossy(&head);
        let mut lines = head.split("\r\n");
        let status: u16 = lines
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|c| c.parse().ok())
            .ok_or_else(|| bad("no status"))?;
        let (mut length, mut location, mut chunked) = (None::<u64>, None::<String>, false);
        for l in lines {
            if let Some((k, v)) = l.split_once(':') {
                match k.trim().to_ascii_lowercase().as_str() {
                    "content-length" => length = v.trim().parse().ok(),
                    "location" => location = Some(v.trim().to_owned()),
                    "transfer-encoding" => chunked = v.to_ascii_lowercase().contains("chunked"),
                    _ => {}
                }
            }
        }
        match status {
            301 | 302 | 303 | 307 | 308 => {
                let to = location.ok_or_else(|| bad("a redirect without a Location"))?;
                url = resolve(&url, &to);
                continue;
            }
            200 => {}
            s => return Err(bad(format!("{s} from {url}"))),
        }
        if chunked {
            return Err(bad("a chunked answer (not taken for a download)"));
        }
        if let (Some(e), Some(l)) = (expect, length)
            && e.size != l
        {
            return Err(bad(format!(
                "{url}: {l} bytes offered, {} expected (not the pinned file)",
                e.size
            )));
        }
        let mut hash = sha2::Sha256::new();
        // A part file of this download's own (two programs fetching the same file wrote
        // over each other's, and the one whose own bytes hashed right renamed a mixed
        // file into place), removed on every way out but the rename (F2-5).
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let mut part = to.as_os_str().to_owned();
        part.push(format!(
            ".{}-{}.part",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let part = std::path::PathBuf::from(part);
        if let Some(d) = to.parent() {
            std::fs::create_dir_all(d)?;
        }
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part)?;
        struct Gone<'a>(&'a std::path::Path, bool);
        impl Drop for Gone<'_> {
            fn drop(&mut self) {
                if !self.1 {
                    let _ = std::fs::remove_file(self.0);
                }
            }
        }
        let mut guard = Gone(&part, false);
        let mut buf = vec![0u8; 1 << 16];
        let mut got = 0u64;
        loop {
            if stopped() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "stopped"));
            }
            let n = match s.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                // A TLS connection that ended without its closing message: the end only
                // when the length says so (without one, a cut-off download passed as
                // whole; bug round E, E4-11).
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof && length.is_some() => break,
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    let _ = std::fs::remove_file(&part);
                    return Err(bad(format!("cut off at {got} bytes (no length given)")));
                }
                Err(e) => return Err(e),
            };
            if expect.is_some_and(|e| got + n as u64 > e.size) {
                drop(f);
                let _ = std::fs::remove_file(&part);
                return Err(bad(format!("{url}: longer than the pinned file")));
            }
            f.write_all(&buf[..n])?;
            hash.update(&buf[..n]);
            got += n as u64;
            progress(got, length);
            if length.is_some_and(|l| got >= l) {
                break;
            }
        }
        f.sync_all()?;
        drop(f);
        if length.is_some_and(|l| got != l) {
            let _ = std::fs::remove_file(&part);
            return Err(bad(format!(
                "cut off at {got} of {} bytes",
                length.unwrap_or(0)
            )));
        }
        if let Some(e) = expect {
            let sum: [u8; 32] = hash.finalize().into();
            if got != e.size || sum != e.sha256 {
                let _ = std::fs::remove_file(&part);
                return Err(bad(format!(
                    "{url}: {got} bytes, SHA-256 {}: not the pinned file ({} bytes, {}); not kept",
                    idn_common::hex::hex(&sum),
                    e.size,
                    idn_common::hex::hex(&e.sha256)
                )));
            }
        }
        std::fs::rename(&part, to)?;
        guard.1 = true;
        return Ok(got);
    }
    Err(bad("too many redirects"))
}

/// (TLS, host, port, path, host:port as given) of an `http(s)://` address.
fn split_url(url: &str) -> io::Result<(bool, String, u16, String, String)> {
    // (No control characters: a redirect's `Location` with bare line feeds put headers
    // of the far server's choosing into the next request, to an inner host; bug run 4.
    // A space, as typed in an address, is sent as `%20`.)
    if url.chars().any(char::is_control) {
        return Err(bad("the address has control characters"));
    }
    let (tls, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(bad("the address must start with http:// or https://"));
    };
    let (hostport, path) = rest
        .split_once('/')
        .map_or((rest, "/".to_owned()), |(h, p)| (h, format!("/{p}")));
    if hostport.contains(' ') {
        return Err(bad("the address's host has a space"));
    }
    let path = path.replace(' ', "%20");
    let (host, port) = super::split_host(hostport);
    let port = port.unwrap_or(if tls { 443 } else { 80 });
    Ok((tls, host.to_owned(), port, path, hostport.to_owned()))
}

/// A redirect's `Location` against the address it came from.
fn resolve(from: &str, to: &str) -> String {
    if to.starts_with("http://") || to.starts_with("https://") {
        return to.to_owned();
    }
    let scheme_end = from.find("://").map_or(0, |i| i + 3);
    let host_end = from[scheme_end..]
        .find('/')
        .map_or(from.len(), |i| scheme_end + i);
    if to.starts_with('/') {
        format!("{}{to}", &from[..host_end])
    } else {
        let dir = from
            .rfind('/')
            .filter(|&i| i >= host_end)
            .unwrap_or(host_end);
        format!("{}/{to}", &from[..dir])
    }
}

/// (status, Location, body).
fn exchange(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    insecure: bool,
    timeout: Duration,
    limit: usize,
) -> io::Result<(u16, Option<String>, Vec<u8>)> {
    let (tls, host, port, path, hostport) = split_url(url)?;
    let addr = (host.as_str(), port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| bad(format!("{host}: no address")))?;
    let tcp = TcpStream::connect_timeout(&addr, timeout)?;
    tcp.set_read_timeout(Some(timeout))?;
    tcp.set_write_timeout(Some(timeout))?;
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {hostport}\r\nUser-Agent: idn-open-protocol\r\nConnection: close\r\n"
    );
    for (k, v) in headers {
        if k.contains(['\r', '\n']) || v.contains(['\r', '\n']) {
            return Err(bad("a header can't hold a line break"));
        }
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(b) = body {
        req.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    req.push_str("\r\n");
    let mut raw = Vec::new();
    let cut = if tls {
        let cfg = client_config(insecure)?;
        let name = rustls::pki_types::ServerName::try_from(host.clone()).map_err(bad)?;
        let conn = rustls::ClientConnection::new(Arc::new(cfg), name).map_err(bad)?;
        let mut s = rustls::StreamOwned::new(conn, tcp);
        s.write_all(req.as_bytes())?;
        if let Some(b) = body {
            s.write_all(b)?;
        }
        read_all(&mut s, &mut raw, limit)?
    } else {
        let mut s = tcp;
        s.write_all(req.as_bytes())?;
        if let Some(b) = body {
            s.write_all(b)?;
        }
        read_all(&mut s, &mut raw, limit)?
    };
    parse_full(&raw, cut)
}

/// Reads to the end: whether it ended without TLS's closing notice (then only an answer
/// whose length or chunks say it is whole is taken; F2-6).
fn read_all(s: &mut impl Read, out: &mut Vec<u8>, limit: usize) -> io::Result<bool> {
    let mut buf = [0u8; 16384];
    loop {
        match s.read(&mut buf) {
            Ok(0) => return Ok(false),
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if out.len() > limit + 65536 {
                    return Err(bad(format!("the answer is over {} MiB", limit >> 20)));
                }
            }
            // A server closing TLS without its notice: the bytes so far are the answer, if
            // they say they are all of it.
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(true),
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
fn parse(raw: &[u8]) -> io::Result<(u16, Vec<u8>)> {
    parse_full(raw, false).map(|(s, _, b)| (s, b))
}

/// `cut`: the connection ended without TLS's closing notice.
fn parse_full(raw: &[u8], cut: bool) -> io::Result<(u16, Option<String>, Vec<u8>)> {
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| bad("no answer"))?;
    let head = std::str::from_utf8(&raw[..end]).map_err(|_| bad("the headers aren't text"))?;
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| bad("no status"))?;
    let mut chunked = false;
    let mut length = None;
    let mut location = None;
    for l in lines {
        let Some((k, v)) = l.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        if k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
            chunked = true;
        } else if k == "content-length" {
            length = v.parse::<usize>().ok();
        } else if k == "location" {
            location = Some(v.to_owned());
        }
    }
    let rest = &raw[end + 4..];
    let body = if chunked {
        let mut out = Vec::new();
        let mut p = rest;
        loop {
            let nl = p
                .windows(2)
                .position(|w| w == b"\r\n")
                .ok_or_else(|| bad("bad chunk"))?;
            let size_s = std::str::from_utf8(&p[..nl]).map_err(|_| bad("bad chunk"))?;
            let size = usize::from_str_radix(size_s.split(';').next().unwrap_or("").trim(), 16)
                .map_err(|_| bad("bad chunk size"))?;
            p = &p[nl + 2..];
            if size == 0 {
                break;
            }
            if p.len() < size {
                return Err(bad("the answer was cut off"));
            }
            out.extend_from_slice(&p[..size]);
            p = p.get(size + 2..).unwrap_or(&[]);
        }
        out
    } else {
        match length {
            Some(n) if n <= rest.len() => rest[..n].to_vec(),
            Some(_) => return Err(bad("the answer was cut off")),
            None if cut => return Err(bad("the answer was cut off (no length given)")),
            None => rest.to_vec(),
        }
    };
    Ok((status, location, body))
}

/// A TLS client's settings: the system's roots, or (`insecure`) any certificate (a
/// self-signed one, as the Site Manager's and a plant device's are; tests).
pub fn client_config(insecure: bool) -> io::Result<rustls::ClientConfig> {
    let p = crate::secure::crypto_provider();
    let b = rustls::ClientConfig::builder_with_provider(p.clone())
        .with_safe_default_protocol_versions()
        .map_err(bad)?;
    Ok(if insecure {
        b.dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCert(p)))
            .with_no_client_auth()
    } else {
        b.with_root_certificates(rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        })
        .with_no_client_auth()
    })
}

/// `insecure`: any certificate (a plant device's self-signed one); the connection is
/// still encrypted, not authenticated.
#[derive(Debug)]
struct AnyCert(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AnyCert {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &rustls::pki_types::CertificateDer<'_>,
        d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &rustls::pki_types::CertificateDer<'_>,
        d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn answers_plain_and_chunked() {
        assert_eq!(
            super::parse(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello").unwrap(),
            (200, b"hello".to_vec())
        );
        assert_eq!(
            super::parse(b"HTTP/1.1 201 Created\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n").unwrap(),
            (201, b"abcde".to_vec())
        );
        assert!(super::parse(b"HTTP/1.1 200 OK\r\nContent-Length: 50\r\n\r\nshort").is_err());
        // No length, and TLS ended without its notice: not taken as whole (F2-6).
        let open = b"HTTP/1.1 200 OK\r\n\r\nabc";
        assert_eq!(super::parse_full(open, false).unwrap().2, b"abc");
        assert!(super::parse_full(open, true).is_err());
        assert!(
            super::parse_full(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc", true).is_ok()
        );
        let (s, loc, _) = super::parse_full(
            b"HTTP/1.1 307 Temporary Redirect\r\nLocation: /b?x=1\r\nContent-Length: 0\r\n\r\n",
            false,
        )
        .unwrap();
        assert_eq!((s, loc.as_deref()), (307, Some("/b?x=1")));
    }

    /// A download checked against its pin: the right file kept, a changed one refused and
    /// nothing left behind (bug round E, E4-11).
    #[test]
    fn a_pinned_download_keeps_only_the_right_file() {
        use sha2::Digest;
        use std::io::{Read, Write};
        let body = b"the model's bytes".to_vec();
        let serve = |body: Vec<u8>| {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = l.local_addr().unwrap().port();
            std::thread::spawn(move || {
                let (mut c, _) = l.accept().unwrap();
                let mut b = [0u8; 1024];
                let _ = c.read(&mut b);
                let _ = write!(
                    c,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                );
                let _ = c.write_all(&body);
            });
            format!("http://127.0.0.1:{port}/m.bin")
        };
        let dir = idn_common::testing::TempDir::new("httpc-pin");
        let to = dir.join("m.bin");
        let pin = super::Expected {
            size: body.len() as u64,
            sha256: sha2::Sha256::digest(&body).into(),
        };
        let t = std::time::Duration::from_secs(5);
        // The right bytes: kept.
        let n = super::download(&serve(body.clone()), &to, t, Some(pin), &mut |_, _| {}).unwrap();
        assert_eq!(n, body.len() as u64);
        assert_eq!(std::fs::read(&to).unwrap(), body);
        std::fs::remove_file(&to).unwrap();
        // The same size, one byte changed: refused, nothing kept.
        let mut changed = body.clone();
        changed[0] ^= 1;
        let e = super::download(&serve(changed), &to, t, Some(pin), &mut |_, _| {}).unwrap_err();
        assert!(e.to_string().contains("not the pinned file"), "{e}");
        assert!(!to.exists());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "no .part left");
        // Another size: refused before anything is written.
        let e = super::download(&serve(b"short".to_vec()), &to, t, Some(pin), &mut |_, _| {})
            .unwrap_err();
        assert!(e.to_string().contains("expected"), "{e}");
        assert!(!to.exists());
    }

    /// A download stopped: ended within about a second,
    /// even while the server sends nothing, `Interrupted`, nothing left behind.
    #[test]
    fn a_stopped_download_ends_soon_and_leaves_nothing() {
        use std::io::{Read, Write};
        use std::sync::atomic::{AtomicBool, Ordering};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut b = [0u8; 1024];
            let _ = c.read(&mut b);
            let _ = write!(c, "HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\n\r\n");
            let _ = c.write_all(&[7u8; 1000]);
            // Then nothing, for longer than the test waits.
            std::thread::sleep(std::time::Duration::from_secs(20));
        });
        let dir = idn_common::testing::TempDir::new("httpc-stop");
        let to = dir.join("m.bin");
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(500));
            s2.store(true, Ordering::Relaxed);
        });
        let t0 = std::time::Instant::now();
        let e = super::download_stoppable(
            &format!("http://127.0.0.1:{port}/m.bin"),
            &to,
            std::time::Duration::from_secs(15),
            None,
            &mut |_, _| {},
            Some(&stop),
        )
        .unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::Interrupted, "{e}");
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(3),
            "{:?}",
            t0.elapsed()
        );
        assert!(!to.exists());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "no .part left");
    }

    #[test]
    fn addresses_split() {
        let s = |u: &str| {
            let (tls, host, port, path, _) = super::split_url(u).unwrap();
            (tls, host, port, path)
        };
        assert_eq!(
            s("https://[::1]:8443/a b"),
            (true, "::1".into(), 8443, "/a%20b".into())
        );
        assert_eq!(
            s("http://[fe80::1]"),
            (false, "fe80::1".into(), 80, "/".into())
        );
        assert_eq!(s("https://host"), (true, "host".into(), 443, "/".into()));
        assert!(super::split_url("ftp://x").is_err());
    }

    #[test]
    fn redirects_resolve() {
        use super::resolve;
        assert_eq!(resolve("https://a.com/x/y?z", "/b"), "https://a.com/b");
        assert_eq!(resolve("https://a.com/x/y", "c"), "https://a.com/x/c");
        assert_eq!(resolve("https://a.com", "c"), "https://a.com/c");
        assert_eq!(
            resolve("https://a.com/x", "https://b.org/q"),
            "https://b.org/q"
        );
    }
}
