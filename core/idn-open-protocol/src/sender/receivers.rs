//! Receivers met and told: routes followed, refusals logged and answered, the combined
//! tally echoed, and text written on a receiver's connection.

use super::*;

impl Route {
    /// Sends receivers to `target` (name, address); `None` leaves them where they are
    /// (what receivers do with a cleared route is unknown, §1.8).
    pub fn set(&self, target: Option<(String, SocketAddr)>) {
        let mut now = self.now.lock_ok();
        if now.0 != target {
            now.0 = target;
            now.1 += 1;
            self.changed.notify_all();
        }
    }

    /// The current target.
    pub fn get(&self) -> Option<(String, SocketAddr)> {
        self.now.lock_ok().0.clone()
    }

    /// The target once it has changed after `seen` (or `timeout` passes): (target, count).
    pub(super) fn wait(&self, seen: u64, timeout: Duration) -> (Option<(String, SocketAddr)>, u64) {
        let now = self.now.lock_ok();
        let (now, _) = self
            .changed
            .wait_timeout_while(now, timeout, |n| n.1 == seen)
            .unwrap();
        now.clone()
    }
}

/// Logs a refused connection at most every 10 s (§E.8.5): NDI receivers retry a refused
/// source many times a second.
pub(super) fn log_refusal(msg: &str) {
    // Once per 10 s for each reason (refused peers retry at once; a single limit for all
    // reasons hid every reason but the first). The receiver's port
    // changes on each attempt, so the reason is what follows the address.
    static LAST: Mutex<Vec<(String, std::time::Instant, crate::log::Throttle)>> =
        Mutex::new(Vec::new());
    let key = msg.split_once(": ").map_or(msg, |(_, r)| r).to_owned();
    let mut last = LAST.lock_ok();
    let now = std::time::Instant::now();
    // (A reason quiet for a minute is forgotten, its held count said first.)
    last.retain_mut(|(_, t, th)| {
        let keep = now.duration_since(*t) < Duration::from_secs(60);
        if !keep {
            th.flush();
        }
        keep
    });
    match last.iter_mut().find(|(k, _, _)| *k == key) {
        Some((_, t, th)) => {
            *t = now;
            th.log(msg, "such refusals");
        }
        None => {
            let mut th = crate::log::Throttle::new(Duration::from_secs(10));
            th.log(msg, "such refusals");
            if last.len() < 64 {
                last.push((key, now, th));
            }
        }
    }
}

fn tally_echo((p, v): (bool, bool)) -> String {
    let b = |v: bool| if v { "true" } else { "false" };
    element(
        "ndi_tally_echo",
        &[("on_program", b(p)), ("on_preview", b(v))],
    )
}

impl TallyHub {
    pub(super) fn join(&self, c: &Arc<ClientShared>) {
        self.members.lock_ok().push(Arc::downgrade(c));
    }

    /// After a receiver's tally changed, or a receiver left: the combined tally to every
    /// receiver when it changed. Written from a thread of its own, so a receiver that is
    /// behind (its writer busy) holds up nobody else.
    pub(super) fn update(self: &Arc<Self>) {
        {
            // (Worked out under `sent`: two receivers' changes at once could store the
            // older combination last; bug run 2026-11-25.)
            let mut sent = self.sent.lock_ok();
            let (combined, _) = self.members_now();
            if sent.0 == combined {
                return;
            }
            sent.1 += 1;
            *sent = (combined, sent.1);
        }
        if self.echoing.swap(true, Ordering::AcqRel) {
            return; // the thread writing takes this change in its next round
        }
        let hub = self.clone();
        let spawned = std::thread::Builder::new()
            .name("idn-tally-echo".into())
            .spawn(move || {
                loop {
                    let (t, n) = *hub.sent.lock_ok();
                    if *hub.written.lock_ok() < n {
                        *hub.written.lock_ok() = n;
                        let xml = tally_echo(t);
                        for w in hub.members_now().1 {
                            let _ = send_text(&w, kind::METADATA, &xml);
                        }
                    }
                    hub.echoing.store(false, Ordering::Release);
                    // A change made while the last round wrote: another round, unless a
                    // new thread took it.
                    if hub.sent.lock_ok().1 == *hub.written.lock_ok()
                        || hub.echoing.swap(true, Ordering::AcqRel)
                    {
                        return;
                    }
                }
            });
        if spawned.is_err() {
            self.echoing.store(false, Ordering::Release);
        }
    }

    /// The combined tally of the receivers connected, and their writers.
    fn members_now(&self) -> ((bool, bool), Vec<Arc<Mutex<TcpStream>>>) {
        let mut m = self.members.lock_ok();
        m.retain(|w| w.strong_count() > 0);
        let mut t = (false, false);
        let mut writers = Vec::new();
        for c in m.iter().filter_map(|w| w.upgrade()) {
            let Some(w) = c.writer.lock_ok().clone() else {
                continue;
            };
            let (p, v) = *c.tally.lock_ok();
            t = (t.0 | p, t.1 | v);
            writers.push(w);
        }
        (t, writers)
    }

    /// To a receiver that just connected: the combined tally, when something is on.
    pub(super) fn greet(&self, w: &Mutex<TcpStream>) {
        let t = self.sent.lock_ok().0;
        if t != (false, false) {
            let _ = send_text(w, kind::METADATA, &tally_echo(t));
        }
    }
}

/// A text message on a receiver's connection. A write that fails (or times out part way)
/// leaves the stream cut mid-message: the connection is shut, not written on after it (the
/// receiver read the next message from the middle of this one; bug round F, F5-5).
pub(super) fn send_text(s: &Mutex<TcpStream>, k: u16, xml: &str) -> io::Result<()> {
    let mut w = s.lock_ok();
    let r = w.write_all(&Frame::text(k, 0, xml).encode());
    if r.is_err() {
        let _ = w.shutdown(std::net::Shutdown::Both);
    }
    r
}

/// Why one more session is refused, with `total` open and `mine` of them from its
/// address (`local`: this machine's own): `None` when it is taken. One address is held to
/// 16 so a single machine can't take a sender's every place; this machine's own programs
/// (a multiview, displays, a recorder beside the sender) only to the total (20 receivers
/// on the sender's machine: the last 4 were refused, lab 497-A).
pub(super) fn session_refused(total: usize, mine: usize, local: bool) -> Option<String> {
    if total >= MAX_SESSIONS {
        Some(format!("too many connections ({total} in all)"))
    } else if !local && mine >= MAX_SESSIONS_PER_ADDRESS {
        Some(format!("too many connections from that address ({mine})"))
    } else {
        None
    }
}
/// A refused TCP receiver told why (lab 517-B), by one thread for them all: its first
/// message read (within a second), and an IDN receiver's (it offers `x_idn_ext`) answered
/// with `x_idn_refused` (§E.8.5; an NDI receiver gets nothing it doesn't know). At most
/// 16 wait; more are closed at once, as before.
pub(super) fn tell_refused_later(s: TcpStream, why: String) {
    use std::sync::OnceLock;
    use std::sync::mpsc::{SyncSender, sync_channel};
    static TELLER: OnceLock<Option<SyncSender<(TcpStream, String)>>> = OnceLock::new();
    let teller = TELLER.get_or_init(|| {
        let (tx, rx) = sync_channel::<(TcpStream, String)>(16);
        std::thread::Builder::new()
            .name("idn-refusals".into())
            .spawn(move || {
                for (mut s, why) in rx {
                    // Accepted from a non-blocking listener: on Windows and macOS the
                    // socket is non-blocking too, and the timeout did nothing (bug run
                    // I5-1: the receiver's first message was rarely there yet).
                    let _ = s.set_nonblocking(false);
                    // A second for the whole first message, and 64 KB of it at most (bug
                    // run K7-7: the second was each read's, and a byte every 0.9 s held
                    // this one thread while every other refusal was closed unsaid).
                    let mut r = Deadline {
                        s: &mut s,
                        until: std::time::Instant::now() + Duration::from_secs(1),
                    }
                    .take(64 << 10);
                    let idn = matches!(
                        crate::frame::read_frame(&mut r, &CONTROL_LIMITS),
                        Ok(Some(f)) if f.text_payload().is_ok_and(|t| t.contains("x_idn_ext"))
                    );
                    if idn {
                        crate::secure::tell_refused(&s, &why);
                    }
                }
            })
            .ok()
            .map(|_| tx)
    });
    if let Some(t) = teller {
        let _ = t.try_send((s, why));
    }
}

/// Reads from a socket until a moment, all reads together.
struct Deadline<'a> {
    s: &'a mut TcpStream,
    until: std::time::Instant,
}

impl std::io::Read for Deadline<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self
            .until
            .checked_duration_since(std::time::Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::TimedOut))?;
        self.s.set_read_timeout(Some(left))?;
        self.s.read(buf)
    }
}
