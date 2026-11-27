//! A minimal Discovery Server (spec chapter 8): the source protocol (§8.2) and the
//! receiver protocol (§8.3), one thread per client connection.

use super::ds::{MsgReader, Subject, deobfuscate, obfuscate};
use crate::xml::{self, Element};
use idn_common::sync::LockOk as _;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{self, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// A client's outgoing messages, written by its own thread. Handlers only queue while
/// holding the server state, so a client that stops reading can't stall the others
/// (writing under the state lock froze the whole server behind one sleeping laptop). A
/// client that falls more than [`Outbox::MAX`] behind, or whose writes time out, is
/// disconnected.
struct Outbox {
    q: Mutex<(VecDeque<Vec<u8>>, usize, bool)>,
    ready: Condvar,
    sock: TcpStream,
}

type Writer = Arc<Outbox>;

impl Outbox {
    /// Bytes queued for one client before it is dropped.
    const MAX: usize = 4 << 20;
    const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

    fn start(sock: &TcpStream) -> io::Result<Writer> {
        let w = Arc::new(Outbox {
            q: Mutex::default(),
            ready: Condvar::new(),
            sock: sock.try_clone()?,
        });
        let (out, mut sock) = (w.clone(), sock.try_clone()?);
        sock.set_write_timeout(Some(Self::WRITE_TIMEOUT))?;
        std::thread::spawn(move || {
            loop {
                let msg = {
                    let mut g = out.q.lock_ok();
                    loop {
                        if let Some(m) = g.0.pop_front() {
                            g.1 -= m.len();
                            break Some(m);
                        }
                        if g.2 {
                            break None;
                        }
                        g = out
                            .ready
                            .wait(g)
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                    }
                };
                match msg {
                    Some(m) if sock.write_all(&m).is_ok() => {}
                    // Closed and drained, or the client stopped reading.
                    _ => return out.close(),
                }
            }
        });
        Ok(w)
    }

    /// Queues one message (NUL-terminated on the wire).
    fn push(&self, xml: &str) {
        let mut g = self.q.lock_ok();
        if g.2 {
            return;
        }
        if g.1 + xml.len() + 1 > Self::MAX {
            drop(g);
            return self.close();
        }
        let mut b = xml.as_bytes().to_vec();
        b.push(0);
        g.1 += b.len();
        g.0.push_back(b);
        self.ready.notify_one();
    }

    /// Stops the writer and disconnects the client, which ends its handler too.
    fn close(&self) {
        let mut g = self.q.lock_ok();
        g.2 = true;
        g.0.clear();
        g.1 = 0;
        self.ready.notify_one();
        let _ = self.sock.shutdown(Shutdown::Both);
    }

    /// Lets the writer finish what is queued, then stop.
    fn finish(&self) {
        self.q.lock_ok().2 = true;
        self.ready.notify_one();
    }
}

struct Advert {
    subject: Subject,
    items: Vec<Element>,
    w: Writer,
    /// Listener ids whose monitor requests were forwarded here.
    subs: BTreeSet<String>,
}

struct Listener {
    id: String,
    subject: Subject,
    w: Writer,
}

#[derive(Default)]
struct State {
    next: u64,
    sources: BTreeMap<u64, Element>,
    /// Each finder's connection, and this server's address as that finder reaches it.
    finders: BTreeMap<u64, (Writer, IpAddr)>,
    adverts: BTreeMap<u64, Advert>,
    listeners: BTreeMap<u64, Listener>,
}

type Shared = Arc<Mutex<State>>;

fn send_plain(w: &Writer, xml: &str) {
    w.push(xml);
}

fn send_obf(w: &Writer, xml: &str) {
    w.push(&obfuscate(xml));
}

/// Sets `<address>` (a direct child) to the client's IP address.
fn fill_address(e: &mut Element, ip: IpAddr) {
    match e.children.iter_mut().find(|c| c.name == "address") {
        Some(a) => a.text = ip.to_string(),
        None => e
            .children
            .push(Element::new("address").with_text(&ip.to_string())),
    }
}

fn id_of(e: &Element, subject: Subject) -> String {
    e.child_text(&format!("{}_id", subject.tag())).to_owned()
}

/// Client connections served at once, in all and from one address (a machine running
/// many outputs and inputs, each with its own connections, stays well below). More are
/// closed at once.
const MAX_CONNECTIONS: usize = 1024;
const MAX_PER_ADDRESS: usize = 128;
/// How long a new connection may take to say what it is.
const FIRST_MESSAGE: Duration = Duration::from_secs(10);
/// The first message's largest size: a hello, a query or a trust request.
const FIRST_MAX: usize = 64 << 10;
/// Advertisements one connection may make (NDI apps make one per device), and the
/// largest one kept.
const MAX_ADVERTS: usize = 64;
/// Adverts kept in all (bug run 2026-11-25).
const MAX_ADVERTS_ALL: usize = 8192;
/// A full list sent to one connection at most this often.
const LIST_EVERY: std::time::Duration = std::time::Duration::from_secs(1);
const MAX_ADVERT_BYTES: usize = 16 << 10;

/// Runs the server on `listener` until the process ends.
pub fn serve(listener: TcpListener) -> io::Result<()> {
    serve_with(
        listener,
        Trust::None,
        &std::sync::atomic::AtomicBool::new(false),
    )
}

/// Where trust connections (§E.10.2) go.
#[derive(Clone)]
pub enum Trust {
    /// Nowhere: closed (devices fall back, §E.10.3).
    None,
    /// To this site authority.
    Authority(crate::trust::Authority),
    /// Handed on: the connection and its hello (as read, without the NUL), e.g. through
    /// the site link to the main site's authority (idn-remote.md X.12.2).
    Relay(Arc<dyn Fn(TcpStream, Vec<u8>) + Send + Sync>),
}

/// Runs the server, with site trust as `trust` says, until `stop`.
pub fn serve_with(
    listener: TcpListener,
    trust: Trust,
    stop: &std::sync::atomic::AtomicBool,
) -> io::Result<()> {
    use std::sync::atomic::Ordering;
    let state: Shared = Arc::default();
    let clients: Arc<Mutex<BTreeMap<IpAddr, usize>>> = Arc::default();
    // The connections open, so that stopping closes them too (a server stopped in a
    // process that goes on — a test, a restored Site Manager — would else keep serving
    // its sessions).
    let open: Arc<Mutex<BTreeMap<u64, TcpStream>>> = Arc::default();
    let mut next_conn = 0u64;
    listener.set_nonblocking(true)?;
    while !stop.load(Ordering::Relaxed) {
        let (conn, ip) = match listener.accept() {
            Ok((c, a)) => (c, a.ip()),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(20));
                continue;
            }
            Err(_) => {
                // E.g. out of file descriptors: don't spin.
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
        };
        let _ = conn.set_nonblocking(false);
        // Notice peers that vanish without closing (sleep, power loss), so their sources
        // and devices are removed instead of listed forever.
        crate::transport::keepalive(&conn, Duration::from_secs(10));
        {
            let mut c = clients.lock_ok();
            let total: usize = c.values().sum();
            let mine = c.entry(ip).or_default();
            if total >= MAX_CONNECTIONS || *mine >= MAX_PER_ADDRESS {
                if *mine == 0 {
                    c.remove(&ip);
                }
                continue; // dropped: closed
            }
            *mine += 1;
        }
        let leave = {
            let clients = clients.clone();
            move || {
                let mut c = clients.lock_ok();
                if let Some(n) = c.get_mut(&ip) {
                    *n -= 1;
                    if *n == 0 {
                        c.remove(&ip);
                    }
                }
            }
        };
        next_conn += 1;
        let key = next_conn;
        if let Ok(c) = conn.try_clone() {
            open.lock_ok().insert(key, c);
        }
        let (state, trust, open2) = (state.clone(), trust.clone(), open.clone());
        let leave2 = leave.clone();
        let spawned = std::thread::Builder::new().spawn(move || {
            let _ = handle(conn, state, trust);
            open2.lock_ok().remove(&key);
            leave2();
        });
        if spawned.is_err() {
            // Out of threads: refuse this client, keep serving the others.
            open.lock_ok().remove(&key);
            leave();
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    for (_, c) in std::mem::take(&mut *open.lock_ok()) {
        let _ = c.shutdown(std::net::Shutdown::Both);
    }
    Ok(())
}

fn handle(conn: TcpStream, state: Shared, trust: Trust) -> io::Result<()> {
    let peer = conn.peer_addr()?;
    let here = conn.local_addr()?.ip();
    let raw = conn.try_clone()?;
    let mut r = MsgReader::new(conn);
    let id = {
        let mut s = state.lock_ok();
        s.next += 1;
        s.next
    };
    let Some(first) = r.read_first(FIRST_MESSAGE, FIRST_MAX)? else {
        return Ok(());
    };
    let first = String::from_utf8_lossy(&first).into_owned();
    // Site trust (§E.10.2). The device waits for the answer, so nothing beyond the hello
    // is buffered. Without an authority, close: the device falls back (§E.10.3).
    if first.trim_start().starts_with("<idn_trust") {
        match trust {
            Trust::Authority(a) => a.serve_hello(raw, &first),
            Trust::Relay(f) => f(raw, first.into_bytes()),
            Trust::None => {}
        }
        return Ok(());
    }
    let w = Outbox::start(&raw)?;
    let r2 = dispatch(id, peer, here, &first, &w, &mut r, &state);
    w.finish();
    r2
}

fn dispatch(
    id: u64,
    peer: SocketAddr,
    here: IpAddr,
    first: &str,
    w: &Writer,
    r: &mut MsgReader<TcpStream>,
    state: &Shared,
) -> io::Result<()> {
    let parts = xml::parse_fragment(first).unwrap_or_default();
    let names: Vec<&str> = parts.iter().map(|e| e.name.as_str()).collect();
    match names.as_slice() {
        ["query"] => finder(id, here, w, r, state),
        ["source"] => source(id, peer, parts.into_iter().next().unwrap(), r, state),
        [
            "query",
            subj @ ("receiver" | "source"),
            role @ ("advertiser" | "listener"),
        ] => {
            let subject = if *subj == "receiver" {
                Subject::Receiver
            } else {
                Subject::Source
            };
            receiver_protocol(id, peer, subject, *role == "listener", w, r, state)
        }
        _ => Ok(()),
    }
}

fn sources_xml(s: &State, here: IpAddr) -> String {
    let mut all = Element::new("sources");
    all.children
        .extend(s.sources.values().map(|e| as_seen_from(e, here)));
    all.to_xml()
}

/// A source registered from this server's own machine (over loopback) as a finder on
/// another machine must reach it: at this server's address on that finder's side (it was
/// listed at 127.0.0.1 to every machine).
fn as_seen_from(e: &Element, here: IpAddr) -> Element {
    let mut e = e.clone();
    let local = e
        .child_text("address")
        .trim()
        .parse::<IpAddr>()
        .is_ok_and(|a| a.is_loopback());
    if local && !here.is_loopback() {
        fill_address(&mut e, here);
    }
    e
}

fn finder(
    id: u64,
    here: IpAddr,
    w: &Writer,
    r: &mut MsgReader<TcpStream>,
    state: &Shared,
) -> io::Result<()> {
    {
        let mut s = state.lock_ok();
        send_plain(w, &sources_xml(&s, here));
        s.finders.insert(id, (w.clone(), here));
    }
    // Further `<query/>` messages get the list again. A read error ends the finder like
    // a close does (returning early with `?` left it registered).
    // (At most one full list a second a connection: each was built from every advert
    // under the server's one lock, a 9-byte query in a loop stalling every client; bug
    // run 2026-11-25.)
    let mut listed = std::time::Instant::now();
    while let Some(m) = r.read_msg().unwrap_or(None) {
        if m.as_slice() == b"<query/>" && listed.elapsed() >= LIST_EVERY {
            listed = std::time::Instant::now();
            send_plain(w, &sources_xml(&state.lock_ok(), here));
        }
    }
    state.lock_ok().finders.remove(&id);
    Ok(())
}

fn source(
    id: u64,
    peer: SocketAddr,
    mut e: Element,
    r: &mut MsgReader<TcpStream>,
    state: &Shared,
) -> io::Result<()> {
    let broadcast = |s: &State, tag: &str, e: &Element| {
        for (f, here) in s.finders.values() {
            let msg = Element::new(tag)
                .with_child(as_seen_from(e, *here))
                .to_xml();
            send_plain(f, &msg);
        }
    };
    fill_address(&mut e, peer.ip());
    {
        let mut s = state.lock_ok();
        broadcast(&s, "add_source", &e);
        s.sources.insert(id, e);
    }
    // The registration lasts while the connection is open (§8.2.1).
    while r.read_msg().unwrap_or(None).is_some() {}
    let mut s = state.lock_ok();
    if let Some(e) = s.sources.remove(&id) {
        broadcast(&s, "remove_source", &e);
    }
    Ok(())
}

fn receiver_protocol(
    id: u64,
    peer: SocketAddr,
    subject: Subject,
    is_listener: bool,
    w: &Writer,
    r: &mut MsgReader<TcpStream>,
    state: &Shared,
) -> io::Result<()> {
    let tag = subject.tag();
    if is_listener {
        // A fresh random token of 63 letters and digits per listener connection (§8.3.2);
        // NDI 6.3 listeners close the connection on other lengths.
        const ALNUM: &[u8; 62] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        let lid: String = crate::random::secure_random::<63>()
            .iter()
            .map(|b| ALNUM[*b as usize % 62] as char)
            .collect();
        send_obf(w, &format!("<compatible/><listener_id>{lid}</listener_id>"));
        state.lock_ok().listeners.insert(
            id,
            Listener {
                id: lid,
                subject,
                w: w.clone(),
            },
        );
    } else {
        send_obf(w, "<compatible/>");
        state.lock_ok().adverts.insert(
            id,
            Advert {
                subject,
                items: Vec::new(),
                w: w.clone(),
                subs: BTreeSet::new(),
            },
        );
    }
    let mut listed: Option<std::time::Instant> = None;
    while let Some(m) = r.read_msg().unwrap_or(None) {
        let Some(text) = deobfuscate(&m) else {
            continue;
        };
        let Some(msgs) = xml::parse_fragment(&text) else {
            continue;
        };
        for mut e in msgs {
            let mut s = state.lock_ok();
            if !is_listener {
                if e.name == tag {
                    fill_address(&mut e, peer.ip());
                    let key = id_of(&e, subject);
                    let total_adverts: usize = s.adverts.values().map(|a| a.items.len()).sum();
                    let a = s.adverts.get_mut(&id).unwrap();
                    // The first advertisement per id stands, as with the NDI 6.3 server
                    // (§8.3.9); a new one is pushed bare to that type's listeners.
                    if a.items.iter().any(|x| id_of(x, subject) == key) {
                        continue;
                    }
                    let xml = e.to_xml();
                    if a.items.len() >= MAX_ADVERTS || xml.len() > MAX_ADVERT_BYTES {
                        continue;
                    }
                    // (And in all: per connection only, the adverts reached a gigabyte.)
                    if total_adverts >= MAX_ADVERTS_ALL {
                        continue;
                    }
                    a.items.push(e);
                    for l in s.listeners.values().filter(|l| l.subject == subject) {
                        send_obf(&l.w, &xml);
                    }
                } else if e.name == "monitor_events" {
                    // Route to the listener named inside (§8.3.6): only about the devices
                    // this connection advertises (bug run 2026-11-25: any client told the
                    // Site Manager's monitor anything about any device, real ones too).
                    let own: Vec<String> = s
                        .adverts
                        .get(&id)
                        .map(|a| a.items.iter().map(|x| id_of(x, subject)).collect())
                        .unwrap_or_default();
                    let all_own = e.children_named("listener").all(|l| {
                        l.children
                            .iter()
                            .all(|d| d.attr("id").is_none_or(|i| own.iter().any(|o| o == i)))
                    });
                    if all_own
                        && let Some(lid) = e.child("listener").and_then(|l| l.attr("id"))
                        && let Some(l) = s.listeners.values().find(|l| l.id == lid)
                    {
                        send_obf(&l.w, &e.to_xml());
                    }
                }
                continue;
            }
            let my_lid = s.listeners[&id].id.clone();
            match e.name.as_str() {
                n if n == format!("query_{tag}s")
                    && listed.is_none_or(|t| t.elapsed() >= LIST_EVERY) =>
                {
                    listed = Some(std::time::Instant::now());
                    let mut all = Element::new(&format!("{tag}s"));
                    for a in s.adverts.values().filter(|a| a.subject == subject) {
                        all.children.extend(a.items.iter().cloned());
                    }
                    send_obf(w, &all.to_xml());
                    send_obf(
                        w,
                        &format!(
                            "<monitor_events><events_type>{tag}</events_type></monitor_events>"
                        ),
                    );
                }
                "control_receiver" | "monitor_request" => {
                    let target = id_of(&e, subject);
                    if e.name == "monitor_request" {
                        match e.children.iter_mut().find(|c| c.name == "listener_id") {
                            Some(l) => l.text = my_lid.clone(),
                            None => e
                                .children
                                .push(Element::new("listener_id").with_text(&my_lid)),
                        }
                    }
                    let xml = e.to_xml();
                    let is_monitor = e.name == "monitor_request";
                    if let Some(a) = s.adverts.values_mut().find(|a| {
                        a.subject == subject && a.items.iter().any(|x| id_of(x, subject) == target)
                    }) {
                        send_obf(&a.w, &xml);
                        if is_monitor {
                            a.subs.insert(my_lid.clone());
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let mut s = state.lock_ok();
    if is_listener {
        if let Some(l) = s.listeners.remove(&id) {
            for a in s.adverts.values_mut() {
                if a.subs.remove(&l.id) {
                    send_obf(
                        &a.w,
                        &format!("<remove_listener>{}</remove_listener>", l.id),
                    );
                }
            }
        }
    } else if let Some(a) = s.adverts.remove(&id) {
        // Listeners of that type get the advertisements back in a removal (§8.3.8).
        if !a.items.is_empty() {
            let mut gone = Element::new(&format!("remove_{tag}s"));
            gone.children = a.items;
            let xml = gone.to_xml();
            for l in s.listeners.values().filter(|l| l.subject == a.subject) {
                send_obf(&l.w, &xml);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source registered over loopback is listed at the server's address to finders on
    /// other machines, and as it is to finders on this one.
    #[test]
    fn local_sources_get_the_servers_address() {
        let mut e = Element::new("source").with_child(Element::new("name").with_text("A"));
        fill_address(&mut e, "127.0.0.1".parse().unwrap());
        let lan: IpAddr = "10.0.0.5".parse().unwrap();
        assert_eq!(as_seen_from(&e, lan).child_text("address"), "10.0.0.5");
        let lo: IpAddr = "127.0.0.1".parse().unwrap();
        assert_eq!(as_seen_from(&e, lo).child_text("address"), "127.0.0.1");
        let mut far = e.clone();
        fill_address(&mut far, "10.0.0.9".parse().unwrap());
        assert_eq!(as_seen_from(&far, lan).child_text("address"), "10.0.0.9");
    }
    use std::io::Read;

    fn send(s: &mut TcpStream, xml: &str) {
        s.write_all(xml.as_bytes()).unwrap();
        s.write_all(&[0]).unwrap();
    }

    #[test]
    fn a_client_that_stops_reading_does_not_stall_the_server() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || serve(l));
        let pad = "x".repeat(1000);
        let mut regs = Vec::new();
        for i in 0..50 {
            let mut s = TcpStream::connect(addr).unwrap();
            send(&mut s, &format!("<source><name>S{i} {pad}</name></source>"));
            regs.push(s);
        }
        std::thread::sleep(Duration::from_millis(200));
        // A finder that keeps asking for the ~50 kB list and never reads the answers:
        // about 50 MB, far more than socket buffers hold.
        let mut stalled = TcpStream::connect(addr).unwrap();
        for _ in 0..1000 {
            if stalled.write_all(b"<query/>\0").is_err() {
                break; // disconnected by the server
            }
        }
        std::thread::sleep(Duration::from_millis(500));
        // Others are still served promptly: a new source registers and a new finder
        // gets the full list.
        let t = std::time::Instant::now();
        let mut late = TcpStream::connect(addr).unwrap();
        send(&mut late, "<source><name>Late</name></source>");
        // Asked until the late source is listed (a single query after a fixed 100 ms
        // sometimes came before the registration was handled, under load).
        let names = loop {
            let mut f = TcpStream::connect(addr).unwrap();
            f.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            send(&mut f, "<query/>");
            let mut list = Vec::new();
            let mut b = [0u8; 65536];
            while !list.ends_with(&[0]) {
                let n = f.read(&mut b).expect("server answers");
                assert!(n > 0, "connection closed");
                list.extend_from_slice(&b[..n]);
            }
            let n = list.windows(5).filter(|w| w == b"<name").count();
            if n == 51 || t.elapsed() >= Duration::from_secs(3) {
                break n;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(names, 51);
        assert!(
            t.elapsed() < Duration::from_secs(3),
            "answered in {:?}",
            t.elapsed()
        );
        // The stalled finder was cut off rather than buffered without limit.
        stalled
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut sink = vec![0u8; 1 << 20];
        let mut total = 0;
        while let Ok(n) = stalled.read(&mut sink) {
            if n == 0 {
                break;
            }
            total += n;
        }
        assert!(total < 20_000_000, "stalled finder got {total} bytes");
    }

    #[test]
    fn one_address_cannot_take_every_connection() {
        let l = TcpListener::bind("0.0.0.0:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || serve(l));
        let from = |ip: &str| {
            let s =
                socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
            s.bind(&format!("{ip}:0").parse::<SocketAddr>().unwrap().into())
                .unwrap();
            s.connect(
                &format!("127.0.0.1:{port}")
                    .parse::<SocketAddr>()
                    .unwrap()
                    .into(),
            )
            .unwrap();
            TcpStream::from(s)
        };
        let held: Vec<_> = (0..MAX_PER_ADDRESS).map(|_| from("127.0.0.1")).collect();
        std::thread::sleep(Duration::from_millis(300));
        let mut over = from("127.0.0.1");
        over.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        assert!(
            matches!(over.read(&mut [0u8; 16]), Ok(0) | Err(_)),
            "closed at once"
        );
        // Another host is still served.
        let mut other = from("127.0.0.2");
        other
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        send(&mut other, "<query/>");
        let mut b = [0u8; 256];
        assert!(other.read(&mut b).unwrap() > 0);
        drop(held);
    }
}
