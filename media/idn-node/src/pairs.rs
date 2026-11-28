//! Linked computers (idn-extensions.md §E.28, idn-node.md §N.32): the machine's links
//! (remembered ones kept in `pairs.xml`, one-time ones in memory until their last session
//! ends); the port where other computers ask to link (announced on mDNS while the machine
//! is in no site), whose requests wait for this machine's person to compare the codes and
//! accept or reject; and asking another computer.

use idn_common::sync::LockOk as _;
use idn_open_protocol::discovery::service::{Found, ServiceAnnouncer};
use idn_open_protocol::trust::pair::{self, Linked, Peer};
use idn_open_protocol::xml::Element;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// A request waiting for this machine's person: who asks, the code, until when, and the
/// answer once given (accept, remember).
struct Incoming {
    by: String,
    code: String,
    until: Instant,
    answer: Option<(bool, bool)>,
}

/// This machine asking another: to whom, the code, and how it stands (`waiting`,
/// `linked`, `rejected`, `failed`) with why.
#[derive(Debug, Clone)]
struct Outgoing {
    to: String,
    code: String,
    state: &'static str,
    reason: String,
    /// This computer's person's word on the code (the same on both screens): the link is
    /// kept only with `Some(true)`; `Some(false)` cancels. The asking side took whoever
    /// accepted (a machine standing in for the one asked, or a code that differed; bug
    /// round E, E3-3).
    confirmed: Option<bool>,
    /// Which request this is: a second request replaced the first while its thread still
    /// waited, which then took the second's confirmation as its own and linked with no
    /// code compared (bug round F, F6-2).
    serial: u64,
}

/// How the last link ended, for the panel.
#[derive(Debug, Clone)]
struct Outcome {
    at: u64,
    ok: bool,
    text: String,
}

/// A one-time link's life: when made, whether used yet, and since when unused.
type Once = (Instant, bool, Option<Instant>);

/// What is told when the links or a request change (the Node gives its programs the new
/// list, the panel and the session agent the request).
pub type Changed = Arc<dyn Fn() + Send + Sync>;

/// This machine as the port answers for it: its name, its device key (SPKI) and whether
/// others may ask it to link; `None` while it is in a site (no links then).
pub type Me = Arc<dyn Fn() -> Option<(String, Vec<u8>, bool)> + Send + Sync>;

/// Requests: at most one each [`REQUEST_GAP`] and [`REQUESTS`] each [`REQUESTS_PER`], so
/// nobody fills the screen with them.
const REQUEST_GAP: Duration = Duration::from_secs(15);
const REQUESTS: usize = 6;
const REQUESTS_PER: Duration = Duration::from_secs(600);
/// A one-time link ends this long after its last session (reconnects within it go on).
const ONCE_GRACE: Duration = Duration::from_secs(15);
/// A one-time link never used lapses after this.
const ONCE_UNUSED: Duration = Duration::from_secs(300);

/// The port a computer takes requests to link on, so that one can be asked by its address
/// alone (another port only when this one is taken: then mDNS says which).
pub const LINK_PORT: u16 = idn_common::ports::PAIR_LINK;

pub struct Pairs {
    path: PathBuf,
    list: Mutex<Vec<Linked>>,
    incoming: Mutex<Option<Incoming>>,
    answered: Condvar,
    outgoing: Mutex<Option<Outgoing>>,
    last: Mutex<Option<Outcome>>,
    /// Outcomes not yet in the Node's log ([`news`](Self::news) takes them).
    news: Mutex<Vec<String>>,
    /// When requests were taken (the last ten minutes').
    asked: Mutex<VecDeque<Instant>>,
    /// A request is being answered (one at a time).
    busy: AtomicBool,
    /// The port, once listening.
    port: Mutex<Option<u16>>,
    /// Sessions using each link, by program (its connection) and fingerprint.
    uses: Mutex<HashMap<(u64, String), usize>>,
    /// One-time links, by fingerprint.
    once: Mutex<HashMap<String, Once>>,
    /// Set when the Node stops: its listener ends and lets go of its port (it never did:
    /// a Node started again in the same process got another port, and peers linking by
    /// address reached the old one's thread; bug run 4).
    stopped: AtomicBool,
}

/// Whether one more connection to the link port is heard: `all` open now, `here` of them
/// from its address.
fn may_hear(all: usize, here: usize) -> bool {
    all < 64 && here < 2
}

impl Pairs {
    /// The links kept at `path`.
    pub fn load(path: PathBuf) -> Arc<Pairs> {
        // (Unreadable or unparsable: set aside and said, not saved over.)
        let list = match idn_common::fsutil::load_saved(&path, |b| {
            idn_open_protocol::xml::parse(std::str::from_utf8(b).ok()?)
        }) {
            Ok(e) => e
                .map(|e| {
                    e.children_named("pair")
                        .filter_map(Linked::from_element)
                        .filter(|l| !l.once)
                        .collect()
                })
                .unwrap_or_default(),
            Err(why) => {
                idn_open_protocol::log::log(&format!("links: {why}"));
                Vec::new()
            }
        };
        Arc::new(Pairs {
            path,
            list: Mutex::new(list),
            incoming: Mutex::new(None),
            answered: Condvar::new(),
            outgoing: Mutex::new(None),
            last: Mutex::new(None),
            news: Mutex::new(Vec::new()),
            asked: Mutex::new(VecDeque::new()),
            busy: AtomicBool::new(false),
            port: Mutex::new(None),
            uses: Mutex::new(HashMap::new()),
            once: Mutex::new(HashMap::new()),
            stopped: AtomicBool::new(false),
        })
    }

    pub fn list(&self) -> Vec<Linked> {
        self.list.lock_ok().clone()
    }

    /// Remembered links only: one-time ones are never saved.
    fn save(&self, list: &[Linked]) {
        let e = list
            .iter()
            .filter(|l| !l.once)
            .fold(Element::new("pairs"), |e, l| e.with_child(l.to_element()));
        // Whole or not at all (written in place, a power cut mid-write lost every link at
        // the next start; bug round F, F6-7). Kept under the list's lock: saves in order.
        if let Err(x) = idn_common::fsutil::write_atomic(&self.path, e.to_xml().as_bytes()) {
            idn_common::log::log_or_stderr(&format!("idn-node: {}: {x}", self.path.display()));
        }
    }

    /// Keeps `peer` (again, with its new name, if it was linked before).
    fn add(&self, peer: &Peer, once: bool) -> Linked {
        let l = Linked {
            fp: peer.fingerprint(),
            name: peer.name.clone(),
            since: idn_common::epoch::secs(),
            once,
        };
        let mut list = self.list.lock_ok();
        list.retain(|x| x.fp != l.fp);
        list.push(l.clone());
        self.save(&list);
        drop(list);
        let mut o = self.once.lock_ok();
        if once {
            o.insert(l.fp.clone(), (Instant::now(), false, None));
        } else {
            o.remove(&l.fp);
        }
        l
    }

    /// Forgets the link with `fp` (whole, or its first 16 digits): whether there was one.
    pub fn remove(&self, fp: &str) -> bool {
        let fp = fp.to_ascii_lowercase();
        let mut list = self.list.lock_ok();
        let before = list.len();
        list.retain(|x| x.fp != fp && pair::short(&x.fp) != fp);
        let gone = list.len() != before;
        if gone {
            self.save(&list);
        }
        gone
    }

    fn said(&self, ok: bool, text: String) {
        // What another computer sent (a refusal's reason) is in it: no line breaks or other
        // control characters, at most 200 characters (bug run I5-4: a peer could write
        // its own lines into the log).
        let text: String = text
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(200)
            .collect();
        // For the log too (it said only "linked computers changed": lab 516-B; and the
        // library's logger, which the Node doesn't set, said nothing: Alpha76).
        self.note(format!("link: {text}"));
        *self.last.lock_ok() = Some(Outcome {
            at: idn_common::epoch::secs(),
            ok,
            text,
        });
    }

    /// A line for the Node's log, at the next change.
    fn note(&self, line: String) {
        let mut n = self.news.lock_ok();
        if n.len() < 64 {
            n.push(line);
        }
    }

    /// The lines for the Node's log since the last call (the change callback writes them).
    pub fn news(&self) -> Vec<String> {
        std::mem::take(&mut *self.news.lock_ok())
    }

    /// The port other computers ask at, once listening.
    pub fn port(&self) -> Option<u16> {
        *self.port.lock_ok()
    }

    /// Program `client`'s sessions using the link with `fp`: `n` now.
    pub fn used(&self, client: u64, fp: &str, n: usize) {
        let mut u = self.uses.lock_ok();
        if n == 0 {
            u.remove(&(client, fp.to_owned()));
        } else {
            u.insert((client, fp.to_owned()), n);
        }
    }

    /// A program gone: its sessions with it.
    pub fn client_gone(&self, client: u64) {
        self.uses.lock_ok().retain(|(c, _), _| *c != client);
    }

    /// One-time links whose last session ended (after the grace) or never used: forgotten.
    fn tick(&self, changed: &Changed) {
        let in_use: HashSet<String> = self
            .uses
            .lock_ok()
            .keys()
            .map(|(_, fp)| fp.clone())
            .collect();
        let now = Instant::now();
        let mut gone = Vec::new();
        for (fp, (made, used, idle)) in self.once.lock_ok().iter_mut() {
            if in_use.contains(fp) {
                *used = true;
                *idle = None;
                continue;
            }
            let over = if *used {
                now.duration_since(*idle.get_or_insert(now)) >= ONCE_GRACE
            } else {
                now.duration_since(*made) >= ONCE_UNUSED
            };
            if over {
                gone.push(fp.clone());
            }
        }
        if gone.is_empty() {
            return;
        }
        let mut o = self.once.lock_ok();
        let mut list = self.list.lock_ok();
        for fp in &gone {
            o.remove(fp);
            if let Some(l) = list.iter().find(|l| l.fp == *fp) {
                let t = format!("the link for this time with {} ended", l.name);
                self.said(true, t);
            }
            list.retain(|l| l.fp != *fp);
        }
        drop((o, list));
        changed();
    }

    /// The answer of this machine's person to the request waiting: accept (`remember`: kept;
    /// else for this time only) or reject.
    /// Answers the request waiting. An acceptance names the code it was given for: the
    /// request it answers, not whichever waits when it arrives (one ended, another came
    /// with the same computer's name, and an answer read from the old one accepted it).
    pub fn answer(&self, accept: bool, remember: bool, code: Option<&str>) -> Result<(), String> {
        let mut i = self.incoming.lock_ok();
        match i.as_mut() {
            Some(r)
                if accept
                    && code.is_none_or(|c| !idn_open_protocol::trust::same_code(c, &r.code)) =>
            {
                Err("that isn't the code of the request waiting (look again)".into())
            }
            Some(r) if r.answer.is_none() => {
                r.answer = Some((accept, remember));
                self.answered.notify_all();
                Ok(())
            }
            _ => Err("no request to link waits".into()),
        }
    }

    /// Stops listening (the Node stopping).
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
    }

    /// Listens for other computers on [`LINK_PORT`] (or a port of its own when that is
    /// taken), from now on: requests to link,
    /// one at a time. Announced on mDNS as this machine while `me` says it may link.
    pub fn listen(self: &Arc<Self>, me: Me, changed: Changed) -> io::Result<u16> {
        // A known port (asked for by address), else any
        // (announced on mDNS either way).
        let l = TcpListener::bind(("0.0.0.0", LINK_PORT))
            .or_else(|_| TcpListener::bind(("0.0.0.0", 0)))?;
        l.set_nonblocking(true)?;
        let port = l.local_addr()?.port();
        *self.port.lock_ok() = Some(port);
        let announcer = ServiceAnnouncer::start(pair::SERVICE).ok();
        let pairs = self.clone();
        std::thread::Builder::new()
            .name("idn-node-pair".into())
            .spawn(move || {
                let mut announced: Option<String> = None;
                // (This machine as it answers (a copy of the settings, the key's SPKI) is
                // looked at once a second for the announcement, and afresh for each
                // request; not at each look for a request.)
                let mut ticked: Option<Instant> = None;
                while !pairs.stopped.load(Ordering::Relaxed) {
                    if ticked.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
                        ticked = Some(Instant::now());
                        // Announced as this machine while it may link (not in a site).
                        let name = me().map(|(n, ..)| n);
                        if name != announced {
                            if let Some(a) = &announcer {
                                a.set(name.iter().map(|n| (n.clone(), port)).collect());
                            }
                            announced = name;
                        }
                        pairs.tick(&changed);
                    }
                    match l.accept() {
                        Ok((s, from)) => {
                            // (Requests heard at once: 64 at most, and 2 from one
                            // address; each waits up to 10 s for its hello. It was 16 in
                            // all: 16 connections from one machine that said nothing
                            // kept every real request out for those 10 s, and for good
                            // if opened again: lab 484-B.)
                            static OPEN: Mutex<
                                Option<std::collections::HashMap<std::net::IpAddr, usize>>,
                            > = Mutex::new(None);
                            let ip = from.ip();
                            {
                                let mut open = OPEN.lock_ok();
                                let open = open.get_or_insert_with(Default::default);
                                let all: usize = open.values().sum();
                                let here = open.get(&ip).copied().unwrap_or(0);
                                if !may_hear(all, here) {
                                    continue;
                                }
                                *open.entry(ip).or_insert(0) += 1;
                            }
                            let closed = move || {
                                if let Some(open) = OPEN.lock_ok().as_mut()
                                    && let Some(n) = open.get_mut(&ip)
                                {
                                    *n = n.saturating_sub(1);
                                    if *n == 0 {
                                        open.remove(&ip);
                                    }
                                }
                            };
                            let (p, w, c) = (pairs.clone(), me(), changed.clone());
                            let spawned = std::thread::Builder::new()
                                .name("idn-node-pair-req".into())
                                .spawn(move || {
                                    if s.set_nonblocking(false).is_ok() {
                                        p.serve(s, from, w.as_ref(), &c);
                                    }
                                    closed();
                                });
                            if spawned.is_err() {
                                closed();
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(100));
                        }
                        Err(_) => std::thread::sleep(Duration::from_secs(1)),
                    }
                }
            })?;
        Ok(port)
    }

    /// One request to link: the codes made, then this machine's person's answer.
    fn serve(
        &self,
        mut s: TcpStream,
        from: SocketAddr,
        who: Option<&(String, Vec<u8>, bool)>,
        changed: &Changed,
    ) {
        let Some((name, spki, requests)) = who else {
            pair::refuse(
                &mut s,
                "this computer is in a site: it uses the site, not links",
            );
            return;
        };
        if !requests {
            pair::refuse(&mut s, "this computer takes no requests to link");
            return;
        }
        // The request heard before the one place is taken: a connection that sends
        // nothing held it, and every real request was refused for as long as it kept
        // reconnecting (bug round E, E3-4).
        let Ok(hello) = pair::first(&mut s) else {
            return;
        };
        if self.busy.swap(true, Ordering::AcqRel) {
            pair::refuse(
                &mut s,
                "another request to link is being answered: try again soon",
            );
            return;
        }
        struct Free<'a>(&'a AtomicBool);
        impl Drop for Free<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _free = Free(&self.busy);
        // Counted once a request to link has come (a connection that sends nothing, a port
        // scanner's, used the allowance up and real requests were "asked too often").
        {
            let now = Instant::now();
            let mut asked = self.asked.lock_ok();
            while asked
                .front()
                .is_some_and(|t| now.duration_since(*t) > REQUESTS_PER)
            {
                asked.pop_front();
            }
            let soon = asked
                .back()
                .is_some_and(|t| now.duration_since(*t) < REQUEST_GAP);
            if soon || asked.len() >= REQUESTS {
                drop(asked);
                pair::refuse(&mut s, "asked too often: try again in a minute");
                return;
            }
            asked.push_back(now);
        }
        let heard = match pair::hear(&mut s, &hello, name, spki) {
            Ok(h) => h,
            Err(e) => {
                self.said(false, format!("{}: {e}", from.ip()));
                changed();
                return;
            }
        };
        let by = format!("{} ({})", heard.peer.name, from.ip());
        *self.incoming.lock_ok() = Some(Incoming {
            by: by.clone(),
            code: heard.code.clone(),
            until: Instant::now() + pair::ANSWER_WITHIN,
            answer: None,
        });
        changed();
        // This machine's person compares the codes and answers.
        let answer = {
            let mut i = self.incoming.lock_ok();
            loop {
                let Some(r) = i.as_ref() else { break None };
                if let Some(a) = r.answer {
                    break Some(a);
                }
                let left = r.until.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break None;
                }
                i = self
                    .answered
                    .wait_timeout(i, left)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0;
            }
        };
        *self.incoming.lock_ok() = None;
        match answer {
            Some((true, remember)) => match pair::accept(&mut s, !remember) {
                // Kept once the asking computer's person confirmed the code there too
                // (kept at once, it stayed also when the other side then cancelled or
                // never confirmed: lab 484-A, 499-A).
                Ok(()) => match pair::confirmed(&mut s) {
                    Ok(true) => {
                        let l = self.add(&heard.peer, !remember);
                        let how = if remember { "" } else { " (for this time)" };
                        self.said(true, format!("linked with {}{how}", l.name));
                    }
                    Ok(false) => self.said(false, format!("{by}: cancelled there")),
                    Err(_) => self.said(
                        false,
                        format!("{by}: the code wasn't confirmed there; not linked"),
                    ),
                },
                Err(e) => self.said(false, format!("{by}: {e}")),
            },
            Some((false, _)) => {
                pair::refuse(&mut s, "rejected");
                self.said(false, format!("{by}: rejected"));
            }
            None => {
                pair::refuse(&mut s, "not answered in time");
                self.said(false, format!("{by}: not answered in time"));
            }
        }
        changed();
    }

    /// Asks the computer at `addr` to link: its name and the code both show; its person's
    /// answer comes later (the outgoing request's state).
    /// This computer's person's word on the request it made: the codes match (`true`, the
    /// link is kept once the other accepts) or not / cancel (`false`).
    /// `code`: the one the person saw (the panel sends it): refused if it is no longer the
    /// request's (another replaced it; F6-2).
    pub fn confirm(&self, ok: bool, code: Option<&str>) -> Result<(), String> {
        let mut o = self.outgoing.lock_ok();
        match o.as_mut() {
            Some(x)
                if ok && code.is_some_and(|c| !idn_open_protocol::trust::same_code(c, &x.code)) =>
            {
                Err("that code is no longer this request's: look at the code shown now".into())
            }
            Some(x) if matches!(x.state, "waiting" | "accepted") => {
                x.confirmed = Some(ok);
                if !ok {
                    x.state = "cancelled";
                    x.reason = "cancelled here".into();
                }
                Ok(())
            }
            _ => Err("no request to link is waiting".into()),
        }
    }

    /// After the other accepted: this computer's person's word on request `serial`, up
    /// to two minutes (`Some(false)` once another request replaced it).
    fn wait_confirmed(&self, serial: u64, changed: &Changed) -> Option<bool> {
        if let Some(o) = self.outgoing.lock_ok().as_mut()
            && o.serial == serial
            && o.confirmed.is_none()
        {
            o.state = "accepted";
        }
        changed();
        let until = Instant::now() + Duration::from_secs(120);
        while Instant::now() < until {
            match self.outgoing.lock_ok().as_ref() {
                Some(o) if o.serial == serial => {
                    if let Some(c) = o.confirmed {
                        return Some(c);
                    }
                }
                _ => return Some(false),
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        None
    }

    pub fn ask(
        self: &Arc<Self>,
        addr: SocketAddr,
        name: &str,
        spki: &[u8],
        changed: Changed,
    ) -> io::Result<(String, String)> {
        // Shown at once, and one ask at a time (bug run I5-5: the panel showed nothing
        // while the other computer answered, and a second click started a second ask).
        {
            let mut o = self.outgoing.lock_ok();
            // One request at a time: not over one asking, waiting or accepted (bug run
            // J4-10: a failed second ask replaced it, and its computer was told
            // "cancelled").
            if o.as_ref()
                .is_some_and(|o| matches!(o.state, "asking" | "waiting" | "accepted"))
            {
                return Err(io::Error::other(
                    "already asking a computer: wait for its answer, or cancel it",
                ));
            }
            *o = Some(Outgoing {
                to: addr.to_string(),
                code: String::new(),
                state: "asking",
                reason: String::new(),
                confirmed: None,
                serial: 0,
            });
        }
        changed();
        let asking = match pair::ask(addr, name, spki) {
            Ok(a) => a,
            Err(e) => {
                if let Some(o) = self.outgoing.lock_ok().as_mut()
                    && o.state == "asking"
                {
                    o.state = "failed";
                    o.reason = e.to_string();
                }
                changed();
                return Err(e);
            }
        };
        let (to, code) = (asking.peer.name.clone(), asking.code.clone());
        static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let serial = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        *self.outgoing.lock_ok() = Some(Outgoing {
            to: to.clone(),
            code: code.clone(),
            state: "waiting",
            reason: String::new(),
            confirmed: None,
            serial,
        });
        changed();
        let me = self.clone();
        let peer = asking.peer.clone();
        std::thread::Builder::new()
            .name("idn-node-pair-ask".into())
            .spawn(move || {
                let mut asking = asking;
                let (state, reason) = match asking.wait() {
                    // Accepted there: kept once this computer's person says the codes
                    // matched (two minutes to), not before; and the other computer told
                    // which it was, so it keeps the link only then too.
                    Ok(once) => {
                        let word = me.wait_confirmed(serial, &changed);
                        // Kept only when the other computer was told (it keeps the link
                        // only then; bug run I5-3: this one kept it anyway).
                        let told = asking.done(word == Some(true));
                        match (word, told) {
                            (Some(true), Ok(())) => {
                                let l = me.add(&peer, once);
                                let how = if once { " (for this time)" } else { "" };
                                me.said(true, format!("linked with {}{how}", l.name));
                                ("linked", String::new())
                            }
                            (Some(true), Err(e)) => {
                                let why = format!("the other computer couldn't be told ({e})");
                                me.said(false, format!("{}: {why}; not linked", peer.name));
                                ("failed", why)
                            }
                            (Some(false), _) => {
                                me.said(false, format!("{}: cancelled here", peer.name));
                                ("cancelled", "cancelled here".to_owned())
                            }
                            (None, _) => {
                                me.said(
                                    false,
                                    format!("{}: the code wasn't confirmed here", peer.name),
                                );
                                (
                                    "cancelled",
                                    "the code wasn't confirmed here in time".to_owned(),
                                )
                            }
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                        me.said(false, format!("{}: {e}", peer.name));
                        ("rejected", e.to_string())
                    }
                    Err(e) => {
                        me.said(false, format!("{}: {e}", peer.name));
                        ("failed", e.to_string())
                    }
                };
                if let Some(o) = me.outgoing.lock_ok().as_mut()
                    && o.serial == serial
                {
                    o.state = state;
                    o.reason = reason;
                }
                changed();
            })?;
        Ok((to, code))
    }

    /// `<pairs on>` with each link; with `panel`, the request waiting here (who, code,
    /// seconds left), this machine's own request and the last outcome too (the panel's;
    /// programs get the links only).
    pub fn element(&self, on: bool, panel: bool) -> Element {
        let mut e = Element::new("pairs").with_attr("on", if on { "true" } else { "false" });
        if let Some(p) = self.port() {
            e = e.with_attr("port", &p.to_string());
        }
        if panel {
            if let Some(r) = self.incoming.lock_ok().as_ref() {
                let left = r.until.saturating_duration_since(Instant::now()).as_secs();
                e = e.with_child(
                    Element::new("incoming")
                        .with_attr("by", &r.by)
                        .with_attr("code", &r.code)
                        .with_attr("left_s", &left.to_string()),
                );
            }
            if let Some(o) = self.outgoing.lock_ok().as_ref() {
                e = e.with_child(
                    Element::new("outgoing")
                        .with_attr("to", &o.to)
                        .with_attr("code", &o.code)
                        .with_attr("state", o.state)
                        .with_attr("reason", &o.reason)
                        .with_attr(
                            "confirmed",
                            match o.confirmed {
                                Some(true) => "true",
                                Some(false) => "false",
                                None => "",
                            },
                        ),
                );
            }
            if let Some(o) = self.last.lock_ok().as_ref() {
                e = e.with_child(
                    Element::new("last")
                        .with_attr("at", &o.at.to_string())
                        .with_attr("ok", if o.ok { "true" } else { "false" })
                        .with_attr("text", &o.text),
                );
            }
        }
        for l in self.list.lock_ok().iter() {
            e = e.with_child(l.to_element());
        }
        e
    }
}

/// The computers that take links on the network now, found within two seconds.
pub fn find() -> Vec<Found> {
    idn_open_protocol::discovery::service::find(pair::SERVICE, Duration::from_secs(2))
}

/// `Found` as the panel and the command line give it.
pub fn found_element(f: &Found) -> Element {
    Element::new("machine")
        .with_attr("name", &f.instance)
        .with_attr("addr", &f.addr.to_string())
        .with_attr("port", &f.port.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    fn wait_for(what: impl Fn() -> bool) -> bool {
        idn_common::testing::wait_until(Duration::from_secs(10), what)
    }

    #[test]
    fn a_request_shows_the_same_code_on_both_and_the_answer_decides() {
        let dir = idn_common::testing::TempDir::new("idn-pairs");
        let key = |_| idn_open_protocol::trust::pki::KeyFile::generate().unwrap();
        let (ka, kb) = (key(0), key(1));
        let in_site = Arc::new(AtomicBool::new(false));
        let a = Pairs::load(dir.join("a.xml"));
        let (spki, site) = (ka.spki(), in_site.clone());
        let me: Me = Arc::new(move || {
            (!site.load(Ordering::Relaxed)).then(|| ("Alpha".to_owned(), spki.clone(), true))
        });
        let told = Arc::new(AtomicU32::new(0));
        let t = told.clone();
        let a_changed: Changed = Arc::new(move || {
            t.fetch_add(1, Ordering::Relaxed);
        });
        let port = a.listen(me, a_changed).unwrap();
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let b = Pairs::load(dir.join("b.xml"));
        let b_changed: Changed = Arc::new(|| {});
        // Asked: both show the same code; A accepts and remembers B.
        let (to, code) = b.ask(addr, "Bravo", &kb.spki(), b_changed.clone()).unwrap();
        assert_eq!(to, "Alpha");
        assert!(wait_for(|| a
            .element(true, true)
            .child("incoming")
            .is_some()));
        let inc = a.element(true, true).child("incoming").cloned().unwrap();
        assert_eq!(inc.attr("code"), Some(code.as_str()));
        assert!(inc.attr("by").unwrap().starts_with("Bravo"));
        // An acceptance with another code isn't taken; with the one shown, it is (typed
        // with its space, as shown: "123 456").
        assert!(a.answer(true, true, Some("000000")).is_err());
        a.answer(
            true,
            true,
            Some(&idn_open_protocol::trust::show_code(&code)),
        )
        .unwrap();
        // B keeps it only once its own person says the codes matched.
        assert!(wait_for(|| b
            .element(true, true)
            .child("outgoing")
            .is_some_and(|o| o.attr("state") == Some("accepted"))));
        assert!(b.list().is_empty());
        b.confirm(true, None).unwrap();
        assert!(wait_for(|| b.list().len() == 1 && a.list().len() == 1));
        assert_eq!(a.list()[0].fp, kb.fingerprint());
        assert!(!a.list()[0].once && !b.list()[0].once);
        assert_eq!(Pairs::load(dir.join("a.xml")).list(), a.list());
        assert!(told.load(Ordering::Relaxed) >= 2);
        // Both have a line for the Node's log (none reached it: lab 516-B), taken once.
        assert_eq!(a.news(), ["link: linked with Bravo"]);
        assert_eq!(b.news(), ["link: linked with Alpha"]);
        assert!(a.news().is_empty());
        // Rejected: nothing kept, B told.
        assert!(a.remove(pair::short(&kb.fingerprint())) && b.remove(&ka.fingerprint()));
        std::thread::sleep(REQUEST_GAP);
        b.ask(addr, "Bravo", &kb.spki(), b_changed.clone()).unwrap();
        assert!(wait_for(|| a
            .element(true, true)
            .child("incoming")
            .is_some()));
        a.answer(false, false, None).unwrap();
        assert!(wait_for(|| {
            b.element(true, true)
                .child("outgoing")
                .is_some_and(|o| o.attr("state") == Some("rejected"))
        }));
        assert!(a.list().is_empty() && b.list().is_empty());
        // Accepted here, then cancelled there (lab 484-A): nothing kept on either side.
        std::thread::sleep(REQUEST_GAP);
        let (_, code) = b.ask(addr, "Bravo", &kb.spki(), b_changed.clone()).unwrap();
        assert!(wait_for(|| a
            .element(true, true)
            .child("incoming")
            .is_some()));
        a.answer(true, false, Some(&code)).unwrap();
        assert!(wait_for(|| b
            .element(true, true)
            .child("outgoing")
            .is_some_and(|o| o.attr("state") == Some("accepted"))));
        assert!(
            a.list().is_empty(),
            "not kept before the other side confirms"
        );
        b.confirm(false, None).unwrap();
        assert!(wait_for(|| {
            b.element(true, true)
                .child("outgoing")
                .is_some_and(|o| o.attr("state") == Some("cancelled"))
        }));
        std::thread::sleep(Duration::from_millis(300));
        assert!(a.list().is_empty() && b.list().is_empty());
        // Asked again at once: too soon.
        assert!(b.ask(addr, "Bravo", &kb.spki(), b_changed.clone()).is_err());
        // In a site: refused.
        in_site.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(300));
        let e = b.ask(addr, "Bravo", &kb.spki(), b_changed).unwrap_err();
        assert!(e.to_string().contains("site"), "{e}");
    }

    /// Lab 484-B: one address's silent connections don't keep another's request out.
    #[test]
    fn one_address_cant_take_every_place() {
        assert!(may_hear(0, 0) && may_hear(1, 1));
        // A third from the same address: no; another address beside those two: yes.
        assert!(!may_hear(2, 2));
        assert!(may_hear(2, 0));
        // Thirty-one addresses with two each: still room; full at 64.
        assert!(may_hear(62, 0));
        assert!(!may_hear(64, 0));
    }

    #[test]
    fn a_link_for_this_time_ends_with_its_last_session() {
        let dir = idn_common::testing::TempDir::new("idn-pairs-once");
        let p = Pairs::load(dir.join("p.xml"));
        let peer = Peer {
            name: "Guest".into(),
            spki: idn_open_protocol::trust::pki::KeyFile::generate()
                .unwrap()
                .spki(),
        };
        let fp = peer.fingerprint();
        let told = Arc::new(AtomicU32::new(0));
        let t = told.clone();
        let changed: Changed = Arc::new(move || {
            t.fetch_add(1, Ordering::Relaxed);
        });
        p.add(&peer, true);
        // Never saved.
        assert!(Pairs::load(dir.join("p.xml")).list().is_empty());
        // In use by a program: kept.
        p.used(7, &fp, 2);
        p.tick(&changed);
        assert_eq!(p.list().len(), 1);
        // Its program gone: kept for the grace, then ended.
        p.client_gone(7);
        p.tick(&changed);
        assert_eq!(p.list().len(), 1);
        p.once.lock_ok().get_mut(&fp).unwrap().2 =
            Some(Instant::now() - ONCE_GRACE - Duration::from_secs(1));
        p.tick(&changed);
        assert!(p.list().is_empty());
        assert_eq!(told.load(Ordering::Relaxed), 1);
    }
}
