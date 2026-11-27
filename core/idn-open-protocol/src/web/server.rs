//! Serving: connection limits, deadlines, TLS, reading a request, WebSocket upgrades.

use super::*;

/// Connections served at once; more are closed at once.
const MAX_CONNECTIONS: usize = 32;
/// Of those, from one address (one host could hold them all).
const MAX_PER_ADDRESS: usize = 8;
/// The longest a request may take to arrive, however slowly its bytes come.
const REQUEST_TIME: Duration = Duration::from_secs(10);

/// Connections now, from each address.
type PerAddress = Arc<std::sync::Mutex<std::collections::HashMap<IpAddr, usize>>>;

/// A connection's place in the counts, given back when its thread ends.
struct Leave(Arc<std::sync::atomic::AtomicUsize>, PerAddress, IpAddr);

impl Drop for Leave {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
        let mut m = self.1.lock_ok();
        if let Some(n) = m.get_mut(&self.2) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.2);
            }
        }
    }
}

/// A socket whose reads all end by one deadline (each read's timeout is the time left):
/// a TLS handshake read a byte at a time under each read's own timeout held a connection
/// for hours.
struct ByDeadline {
    s: TcpStream,
    end: Instant,
}

impl Read for ByDeadline {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        let left = self
            .end
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "request too slow"))?;
        self.s.set_read_timeout(Some(left))?;
        let n = self.s.read(b)?;
        // Each 64 KiB that comes earns a second more: an upload of a large body (a media
        // file, a release) was cut off after 10 s however fast it came; one sent a byte at
        // a time still ends by the first deadline (deep bug run 2026-11-09).
        self.end += Duration::from_secs_f64(n as f64 / 65_536.0);
        Ok(n)
    }
}

impl Write for ByDeadline {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.s.write(b)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.s.flush()
    }
}

/// Serves `listener` until `stop`, each connection on a thread of its own, over TLS when
/// `tls` is given. Returns at once. Request bodies: 64 KiB at most.
pub fn serve(
    listener: TcpListener,
    tls: Option<Arc<rustls::ServerConfig>>,
    handler: Handler,
    stop: Arc<AtomicBool>,
) -> io::Result<()> {
    serve_with(listener, tls, handler, stop, &[])
}

/// [`serve`], with bodies of up to `n` bytes for paths starting with `prefix`, for each
/// `(prefix, n)` of `large` (the Site Manager's release pieces, §N.23, and restores);
/// 64 KiB elsewhere.
pub fn serve_with(
    listener: TcpListener,
    tls: Option<Arc<rustls::ServerConfig>>,
    handler: Handler,
    stop: Arc<AtomicBool>,
    large: &'static [(&'static str, usize)],
) -> io::Result<()> {
    serve_gated(listener, tls, handler, stop, large, None)
}

/// Asked with a request's head (its body empty) before a body over 64 KiB is read: false
/// answers 403 without reading it.
pub type Gate = Arc<dyn Fn(&Request) -> bool + Send + Sync>;

/// The most read of a body before [`serve_gated`]'s gate is asked.
const SMALL_BODY: usize = 64 * 1024;

/// [`serve_with`], a body over 64 KiB read only when `gate` takes the request's head (a
/// session): 32 connections each sending a 64 MiB restore before any login made ~2 GiB.
pub fn serve_gated(
    listener: TcpListener,
    tls: Option<Arc<rustls::ServerConfig>>,
    handler: Handler,
    stop: Arc<AtomicBool>,
    large: &'static [(&'static str, usize)],
    gate: Option<Gate>,
) -> io::Result<()> {
    serve_until(listener, tls, handler, stop, large, gate).map(drop)
}

/// How long a stopped server waits for its connections to end ([`serve_until`]).
const STOP_WAIT: Duration = Duration::from_secs(3);

/// [`serve_gated`], its accept thread returned: once `stop` is set, the listener is closed
/// (its port free), the WebSockets end (within about a second) and the connections are
/// waited for (up to 3 s) before the thread ends, so joining it is a whole stop (bug run
/// O-M5: a host's stop left the port taken and a client's WebSocket keeping the state).
pub fn serve_until(
    listener: TcpListener,
    tls: Option<Arc<rustls::ServerConfig>>,
    handler: Handler,
    stop: Arc<AtomicBool>,
    large: &'static [(&'static str, usize)],
    gate: Option<Gate>,
) -> io::Result<std::thread::JoinHandle<()>> {
    listener.set_nonblocking(true)?;
    // A panel on 127.0.0.1 answers only to its own address by name: a web page whose name
    // was made to point at 127.0.0.1 (DNS rebinding) is its own origin to the browser, and
    // got past `may_change`. Panels on the LAN are unaffected.
    let handler = match listener.local_addr() {
        Ok(a) if a.ip().is_loopback() => {
            let port = a.port();
            let inner = handler;
            let own: Handler = Arc::new(move |r: Request| {
                if loopback_host(r.header("host"), port) {
                    inner(r)
                } else {
                    Response::text("421 Misdirected Request", "wrong host")
                }
            });
            own
        }
        _ => handler,
    };
    let open = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let per: PerAddress = Arc::default();
    std::thread::Builder::new()
        .name("idn-web".into())
        .spawn(move || {
            let listener = listener;
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((s, peer)) => {
                        // A thread per connection, but only so many at once: a LAN panel
                        // must not be made to hold threads without end.
                        if open.fetch_add(1, Ordering::Relaxed) >= MAX_CONNECTIONS {
                            open.fetch_sub(1, Ordering::Relaxed);
                            drop(s);
                            continue;
                        }
                        // Per address, except this machine's own (its browser's six
                        // connections, the tray and tools at once, a proxy in front);
                        // over it, told so (without TLS: a TLS client can't read it).
                        {
                            let mut m = per.lock_ok();
                            let n = m.entry(peer.ip()).or_insert(0);
                            if *n >= MAX_PER_ADDRESS && !peer.ip().is_loopback() {
                                drop(m);
                                open.fetch_sub(1, Ordering::Relaxed);
                                if tls.is_none() {
                                    let _ = s.set_write_timeout(Some(Duration::from_millis(200)));
                                    let _ = (&s).write_all(
                                        b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                                    );
                                }
                                drop(s);
                                continue;
                            }
                            *n += 1;
                        }
                        let (tls, handler, gate, stop) =
                            (tls.clone(), handler.clone(), gate.clone(), stop.clone());
                        let leave = Leave(open.clone(), per.clone(), peer.ip());
                        let started = std::thread::Builder::new()
                            .name("idn-web-conn".into())
                            .spawn(move || {
                                // Counted out however it ends, a handler's panic included
                                // (32 of those would have closed the panel for good).
                                let _done = leave;
                                let _ = connection(
                                    s,
                                    peer.ip(),
                                    tls,
                                    &handler,
                                    large,
                                    gate.as_ref(),
                                    &stop,
                                );
                            });
                        // (A thread that didn't start dropped its `leave` with it.)
                        drop(started);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(200)),
                }
            }
            // Stopped: the port free at once, the connections (their WebSockets told by
            // `stop`) waited for.
            drop(listener);
            let end = Instant::now() + STOP_WAIT;
            while open.load(Ordering::Relaxed) > 0 && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(20));
            }
        })
}

fn connection(
    s: TcpStream,
    peer: IpAddr,
    tls: Option<Arc<rustls::ServerConfig>>,
    handler: &Handler,
    large: &'static [(&'static str, usize)],
    gate: Option<&Gate>,
    stop: &Arc<AtomicBool>,
) -> io::Result<()> {
    s.set_nonblocking(false)?;
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    s.set_write_timeout(Some(Duration::from_secs(10)))?;
    // The handshake and the request, all within REQUEST_TIME.
    let s = ByDeadline {
        s,
        end: Instant::now() + REQUEST_TIME,
    };
    match tls {
        Some(cfg) => {
            let conn = rustls::ServerConnection::new(cfg).map_err(io::Error::other)?;
            let mut stream = rustls::StreamOwned::new(conn, s);
            match read_request(&mut stream, peer, true, large, gate) {
                Ok(r) => {
                    let resp = handler(r);
                    if let Some((run, still)) = taken(&resp) {
                        resp.write_upgrade(&mut stream)?;
                        let rustls::StreamOwned { conn, sock } = stream;
                        let plain = sock.s;
                        plain.set_read_timeout(Some(events::TICK))?;
                        let io = Box::new(rustls::StreamOwned::new(conn, plain));
                        run(socket(io, until_stopped(still, stop)));
                        return Ok(());
                    }
                    resp.write(&mut stream)?
                }
                Err(e) if refused(&e) => too_large(&mut stream, &e)?,
                Err(e) => return Err(e),
            }
            stream.conn.send_close_notify();
            let _ = stream.flush();
            Ok(())
        }
        None => {
            let mut stream = s;
            match read_request(&mut stream, peer, false, large, gate) {
                Ok(r) => {
                    let resp = handler(r);
                    if let Some((run, still)) = taken(&resp) {
                        resp.write_upgrade(&mut stream)?;
                        let plain = stream.s;
                        plain.set_read_timeout(Some(events::TICK))?;
                        run(socket(Box::new(plain), until_stopped(still, stop)));
                        return Ok(());
                    }
                    resp.write(&mut stream)
                }
                Err(e) if refused(&e) => too_large(&mut stream, &e),
                Err(e) => Err(e),
            }
        }
    }
}

/// What keeps a WebSocket going: `still` (when given), and its server not stopped.
fn until_stopped(still: Option<StillAllowed>, stop: &Arc<AtomicBool>) -> Option<StillAllowed> {
    let stop = stop.clone();
    Some(match still {
        Some(still) => Arc::new(move || !stop.load(Ordering::Relaxed) && still()),
        None => Arc::new(move || !stop.load(Ordering::Relaxed)),
    })
}

/// A response's WebSocket to run, once, and what keeps it allowed.
fn taken(resp: &Response) -> Option<(RunSocket, Option<StillAllowed>)> {
    let u = resp.upgrade.as_ref()?;
    Some((u.0.lock_ok().take()?, u.1.clone()))
}

/// An upgraded connection as a server's WebSocket: messages and frames of 1 MiB at most
/// (bug run O2-L8: tungstenite's 64 MiB passed by the bodies' 64 KiB), ended as soon as
/// `still` says it may no longer go on (looked at about once a second).
fn socket(io: Box<dyn Duplex>, still: Option<StillAllowed>) -> WebSocket {
    let io: Box<dyn Duplex> = match still {
        Some(still) => Box::new(Checked {
            io,
            still,
            looked: Instant::now(),
        }),
        None => io,
    };
    let cfg = tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(1 << 20))
        .max_frame_size(Some(1 << 20));
    tungstenite::WebSocket::from_raw_socket(io, tungstenite::protocol::Role::Server, Some(cfg))
}

/// A connection that fails once `still` says no (looked at about once a second).
struct Checked {
    io: Box<dyn Duplex>,
    still: StillAllowed,
    looked: Instant,
}

impl Checked {
    fn allowed(&mut self) -> io::Result<()> {
        if self.looked.elapsed() >= Duration::from_secs(1) {
            self.looked = Instant::now();
            if !(self.still)() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "the session ended",
                ));
            }
        }
        Ok(())
    }
}

impl Read for Checked {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.allowed()?;
        self.io.read(b)
    }
}

impl Write for Checked {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.allowed()?;
        self.io.write(b)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.io.flush()
    }
}

/// A body over the limit: 413 and why, as the API's errors are (the connection was closed
/// with no answer: an upload in 20 MB pieces got an empty reply).
/// A body not read (too large, or the gate said no): answered, with why.
fn refused(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::FileTooLarge | io::ErrorKind::PermissionDenied
    )
}

fn too_large(s: &mut impl Write, e: &io::Error) -> io::Result<()> {
    let body = serde_json::json!({"ok": false, "error": e.to_string()}).to_string();
    let status = if e.kind() == io::ErrorKind::PermissionDenied {
        "403 Forbidden"
    } else {
        "413 Payload Too Large"
    };
    write!(
        s,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    s.flush()
}

pub(super) fn read_request(
    s: &mut impl Read,
    peer: IpAddr,
    tls: bool,
    large: &'static [(&'static str, usize)],
    gate: Option<&Gate>,
) -> io::Result<Request> {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];
    // Each read has its own timeout; this bounds them all (a byte every few seconds
    // kept a connection, and its thread, for hours).
    let deadline = Instant::now() + REQUEST_TIME;
    let late = || {
        (Instant::now() > deadline)
            .then(|| io::Error::new(io::ErrorKind::TimedOut, "request too slow"))
    };
    let head_end = loop {
        if let Some(e) = late() {
            return Err(e);
        }
        let n = s.read(&mut tmp)?;
        if n == 0 {
            return Err(io::Error::other("closed"));
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > 16 * 1024 {
            return Err(io::Error::other("request head too large"));
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let method = first.next().unwrap_or("").to_owned();
    let target = first.next().unwrap_or("/").to_owned();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_owned(), q.to_owned()),
        None => (target, String::new()),
    };
    let mut headers: HashMap<String, String> = HashMap::new();
    for (k, v) in lines.filter_map(|l| l.split_once(':')) {
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        // Cookies in several headers (as HTTP/2 splits them, passed on by a proxy) all
        // kept, joined as one; any other header given twice: the last.
        if k == "cookie"
            && let Some(c) = headers.get_mut(&k)
        {
            c.push_str("; ");
            c.push_str(v);
            continue;
        }
        headers.insert(k, v.to_owned());
    }
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let most = large
        .iter()
        .find(|(prefix, _)| path.starts_with(prefix))
        .map_or(64 * 1024, |l| l.1);
    if len > most {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            format!("a body of {len} bytes: at most {most} here (send it in smaller pieces)"),
        ));
    }
    if len > SMALL_BODY
        && let Some(g) = gate
    {
        let head = Request {
            method: method.clone(),
            path: path.clone(),
            query: query.clone(),
            headers: headers.clone(),
            body: Vec::new(),
            peer,
            tls,
        };
        if !g(&head) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "log in first (a body this large only in a session)",
            ));
        }
    }
    // The body (one that passed the size and the gate): the request's time plus a second
    // a megabyte, so a large one (a 64 MiB restore) arrives over a slow link too.
    let body_deadline = Instant::now() + REQUEST_TIME + Duration::from_secs(len as u64 / 1_000_000);
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        if Instant::now() > body_deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "request too slow"));
        }
        let n = s.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(len);
    Ok(Request {
        method,
        path,
        query,
        headers,
        body,
        peer,
        tls,
    })
}
