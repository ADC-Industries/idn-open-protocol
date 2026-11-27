//! Relayed key requests (idn-node.md §N.42.3): a client pairing at a
//! machine of the site that doesn't run the Site Manager asks a key through it. The machine
//! only relays: it sends `idn_key_request` over its trust connection, the authority holds
//! the request for the host (the program running the Site Manager) to answer, and sends
//! the answer back as `idn_key_result`. The relay creates and revokes nothing.

use super::*;

/// A trust connection's writer (the authority's, per connection).
type Writer = Arc<Mutex<TcpStream>>;

/// How long a relayed request waits for its answer.
pub const KEY_REQUEST_LIFE: Duration = Duration::from_secs(600);
/// At most this many wait at once (and this many from one connection).
const MAX_WAITING: usize = 64;
const MAX_PER_CONNECTION: usize = 8;

/// How long a relay's pairing code lasts; and how many wrong tries burn it.
pub const PAIRING_CODE_LIFE: Duration = Duration::from_secs(300);
const PAIRING_CODE_TRIES: u32 = 5;

/// A relay's pairing code, as the authority holds it.
#[derive(Debug, Clone)]
pub(crate) struct PairingCode {
    code: String,
    until: Instant,
    wrong: u32,
}

/// A relayed request waiting, as the authority holds it.
#[derive(Debug, Clone)]
pub(crate) struct Pending {
    /// The relay's own number for it (in its answer).
    req: String,
    conn: u64,
    via: String,
    via_fingerprint: String,
    client: String,
    pin: Option<String>,
    code_ok: bool,
    asked: Instant,
    asked_at: u64,
}

/// A relayed key request waiting for the host ([`Authority::key_requests`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRequest {
    /// Its id, for [`Authority::answer_key_request`].
    pub id: u64,
    /// The client's name for people (`Stage iPad`).
    pub client: String,
    /// The machine it came through: its name.
    pub via: String,
    /// And its fingerprint.
    pub via_fingerprint: String,
    /// The PIN the client typed, if any.
    pub pin: Option<String>,
    /// That PIN was the pairing code this site gave the relay, checked here (and used up).
    pub code_ok: bool,
    /// When it was asked (Unix seconds).
    pub asked_at: u64,
}

/// Where a relayed pairing stands, as the relay sees it ([`crate::trust::Enrollment::
/// key_status`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyStatus {
    /// Waiting for the host. `code_wrong`: the site said the code typed with it was wrong
    /// (the client may try again; this request still waits for a person's approval).
    Waiting {
        /// The typed code was refused.
        code_wrong: bool,
    },
    /// Answered (taken once): the key, or why not.
    Answered(KeyGrant),
}

/// The answer to a relayed request, as the relay gets it ([`crate::trust::Enrollment::
/// key_answer`]): approved with the key, or refused with why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyGrant {
    /// Approved: the key follows.
    pub approved: bool,
    /// The key's id (to revoke it by); empty when refused.
    pub id: String,
    /// The key itself (`idn1.…`); empty when refused.
    pub key: String,
    /// Its scope (`control`, `read`, …).
    pub scope: String,
    /// Its audience engine (`app:myapp`).
    pub engine: String,
    /// Why not (refused, or the request ended unanswered).
    pub error: String,
}

impl KeyGrant {
    /// A refusal.
    pub fn refused(why: &str) -> KeyGrant {
        KeyGrant {
            approved: false,
            id: String::new(),
            key: String::new(),
            scope: String::new(),
            engine: String::new(),
            error: why.to_owned(),
        }
    }

    pub(crate) fn from_element(m: &Element) -> KeyGrant {
        let a = |k: &str| m.attr(k).unwrap_or("").to_owned();
        KeyGrant {
            approved: m.attr("ok") == Some("true"),
            id: a("id"),
            key: a("key"),
            scope: a("scope"),
            engine: a("engine"),
            error: a("error"),
        }
    }

    pub(crate) fn element(&self, req: &str) -> Element {
        let e = Element::new("idn_key_result")
            .with_attr("req", req)
            .with_attr("ok", if self.approved { "true" } else { "false" });
        if self.approved {
            e.with_attr("id", &self.id)
                .with_attr("key", &self.key)
                .with_attr("scope", &self.scope)
                .with_attr("engine", &self.engine)
        } else {
            e.with_attr("error", &self.error)
        }
    }
}

/// Text from a relay, for people: at most `n` characters, no control characters.
fn text(v: Option<&str>, n: usize) -> String {
    v.unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(n)
        .collect::<String>()
        .trim()
        .to_owned()
}

impl Authority {
    /// A relay's `idn_key_request` on connection `conn` from `info`: held, and told as
    /// [`AuthorityEvent::KeyRequested`]; or refused at once (the reply to send back).
    pub(super) fn key_requested(&self, m: &Element, conn: u64, info: &CertInfo) -> Option<Element> {
        let req = text(m.attr("req"), 32);
        let client = text(m.attr("client"), 64);
        let pin = Some(text(m.attr("pin"), 32)).filter(|p| !p.is_empty());
        if req.is_empty() || client.is_empty() {
            return Some(KeyGrant::refused("a request number and the client's name").element(&req));
        }
        let req_echo = req.clone();
        self.expire_key_requests();
        // Room first: a right code typed into a request refused for room would be used
        // up for nothing (checked again below, under the same lock as the insert).
        {
            let st = self.0.state.lock_ok();
            let from_here = st.key_requests.values().filter(|p| p.conn == conn).count();
            if st.key_requests.len() >= MAX_WAITING || from_here >= MAX_PER_CONNECTION {
                return Some(KeyGrant::refused("too many pairings waiting").element(&req));
            }
        }
        // (Only through a pairing relay: any other machine could have asked for its own
        // code and verified its own request.)
        let code_ok = self.is_relay(&info.fingerprint)
            && pin
                .as_deref()
                .is_some_and(|p| self.pairing_code_matches(&info.fingerprint, p));
        let id = {
            let mut st = self.0.state.lock_ok();
            let from_here = st.key_requests.values().filter(|p| p.conn == conn).count();
            if st.key_requests.len() >= MAX_WAITING || from_here >= MAX_PER_CONNECTION {
                return Some(KeyGrant::refused("too many pairings waiting").element(&req));
            }
            let id = st.next_key_request;
            st.next_key_request += 1;
            st.key_requests.insert(
                id,
                Pending {
                    req,
                    conn,
                    via: info.name.clone(),
                    via_fingerprint: info.fingerprint.clone(),
                    client: client.clone(),
                    pin: pin.clone(),
                    code_ok,
                    asked: Instant::now(),
                    asked_at: idn_common::epoch::secs(),
                },
            );
            id
        };
        self.event(AuthorityEvent::KeyRequested {
            id,
            code_ok,
            client,
            via: info.name.clone(),
            via_fingerprint: info.fingerprint.clone(),
            pin: pin.is_some(),
        });
        // A code typed and wrong: the relay told at once (its client can try again; the
        // request still waits for a person's approval). Five wrong tries burn the code.
        (pin.is_some() && !code_ok).then(|| {
            Element::new("idn_key_status")
                .with_attr("req", &req_echo)
                .with_attr("code", "wrong")
        })
    }

    /// The relayed key requests waiting for an answer (idn-node.md §N.42.3), oldest first.
    pub fn key_requests(&self) -> Vec<KeyRequest> {
        self.expire_key_requests();
        self.0
            .state
            .lock_ok()
            .key_requests
            .iter()
            .map(|(id, p)| KeyRequest {
                id: *id,
                client: p.client.clone(),
                via: p.via.clone(),
                via_fingerprint: p.via_fingerprint.clone(),
                pin: p.pin.clone(),
                code_ok: p.code_ok,
                asked_at: p.asked_at,
            })
            .collect()
    }

    /// The pairing code for relay `fingerprint` to show: six digits,
    /// made here, good for [`PAIRING_CODE_LIFE`]; the same one while it lasts, a new one
    /// once it is used, burnt (5 wrong tries) or old. With when it ends (Unix seconds).
    /// Sent to the relay when it asks (`idn_pairing_code`); a client typing it into a
    /// relayed pairing makes [`KeyRequest::code_ok`].
    pub fn pairing_code_for(&self, fingerprint: &str) -> (String, u64) {
        let (code, until, _) = self.pairing_code_left(fingerprint);
        (code, until)
    }

    /// [`pairing_code_for`](Self::pairing_code_for), with the seconds it has left too (a
    /// relay whose clock differs from this machine's counts down from that).
    pub fn pairing_code_left(&self, fingerprint: &str) -> (String, u64, u64) {
        let mut st = self.0.state.lock_ok();
        let now = Instant::now();
        let life = st.pairing_code_life;
        let fresh = st
            .pairing_codes
            .get(fingerprint)
            .is_none_or(|c| c.until <= now || c.wrong >= PAIRING_CODE_TRIES);
        if fresh {
            st.pairing_codes.insert(
                fingerprint.to_owned(),
                PairingCode {
                    // (Every code as likely: `% 1_000_000` on a random u32 made the low
                    // ones a little more likely.)
                    code: crate::random::code_digits(6),
                    until: now + life,
                    wrong: 0,
                },
            );
        }
        let c = &st.pairing_codes[fingerprint];
        let left = c.until.saturating_duration_since(now).as_secs();
        (c.code.clone(), idn_common::epoch::secs() + left, left)
    }

    /// How long a pairing code lasts (tests: shorter than [`PAIRING_CODE_LIFE`]).
    #[doc(hidden)]
    pub fn set_pairing_code_life(&self, life: Duration) {
        self.0.state.lock_ok().pairing_code_life = life;
    }

    /// Whether `typed` is relay `fingerprint`'s pairing code now: right, it is used up
    /// (the next pairing gets a new one); wrong, one of its tries goes.
    fn pairing_code_matches(&self, fingerprint: &str, typed: &str) -> bool {
        let typed: String = typed.chars().filter(char::is_ascii_digit).collect();
        let mut st = self.0.state.lock_ok();
        let now = Instant::now();
        let Some(c) = st.pairing_codes.get_mut(fingerprint) else {
            return false;
        };
        if c.until <= now || c.wrong >= PAIRING_CODE_TRIES {
            return false;
        }
        let right = super::proto::same_code(&typed, &c.code);
        if right {
            st.pairing_codes.remove(fingerprint);
        } else {
            c.wrong += 1;
        }
        right
    }

    /// Answers relayed request `id`: the grant (or refusal) sent back through its machine.
    /// Fails when there is no such request, or its machine is gone (then nothing reached
    /// the client: a key made for it should be revoked).
    pub fn answer_key_request(&self, id: u64, grant: &KeyGrant) -> io::Result<()> {
        let (p, w) = {
            let mut st = self.0.state.lock_ok();
            let p = st.key_requests.remove(&id).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("no key request {id}"))
            })?;
            let w = st
                .online
                .iter()
                .find(|(_, c, _)| *c == p.conn)
                .map(|(_, _, w)| w.clone());
            (p, w)
        };
        let w = w.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                format!("{} is gone: the pairing didn't reach its client", p.via),
            )
        })?;
        proto::send(&mut *w.lock_ok(), &grant.element(&p.req).to_xml())?;
        // Kept a while: its client may have taken the request back as it went (then the
        // key is nobody's; bug run 2026-11-24).
        if grant.approved && !grant.id.is_empty() {
            let mut st = self.0.state.lock_ok();
            st.key_answered
                .retain(|(.., at)| at.elapsed() < KEY_REQUEST_LIFE);
            if st.key_answered.len() >= MAX_WAITING * 4 {
                st.key_answered.remove(0);
            }
            st.key_answered
                .push((p.conn, p.req, grant.id.clone(), Instant::now()));
        }
        Ok(())
    }

    /// Keys made for relayed pairings whose client took the request back just as the key
    /// was sent (its relay dropped it: nobody holds it). The host revokes them; each is
    /// given once.
    pub fn take_unclaimed_keys(&self) -> Vec<String> {
        std::mem::take(&mut self.0.state.lock_ok().key_unclaimed)
    }

    /// Requests whose connection closed (`conn`), or older than [`KEY_REQUEST_LIFE`]:
    /// dropped, each told as [`AuthorityEvent::KeyRequestCancelled`].
    pub(super) fn drop_key_requests(&self, conn: Option<u64>) {
        let gone: Vec<(u64, Pending, Option<Writer>)> = {
            let mut st = self.0.state.lock_ok();
            let ids: Vec<u64> = st
                .key_requests
                .iter()
                .filter(|(_, p)| {
                    conn.is_some_and(|c| c == p.conn) || p.asked.elapsed() >= st.key_request_life
                })
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter()
                .filter_map(|id| {
                    let p = st.key_requests.remove(&id)?;
                    // Its relay, while still connected (an expired request's).
                    let w = st
                        .online
                        .iter()
                        .find(|(_, c, _)| *c == p.conn && Some(p.conn) != conn)
                        .map(|(_, _, w)| w.clone());
                    Some((id, p, w))
                })
                .collect()
        };
        for (id, p, w) in gone {
            // The relay told, so its client stops waiting (the request ran out of time).
            if let Some(w) = w {
                let no = KeyGrant::refused("ended unanswered (10 minutes): pair again");
                let _ = proto::send(&mut *w.lock_ok(), &no.element(&p.req).to_xml());
            }
            self.event(AuthorityEvent::KeyRequestCancelled(id));
        }
    }

    /// A relay's `idn_key_withdraw` on connection `conn`: its request `req` taken back (its
    /// client typed a wrong code and asks again: the first stayed on the host's list, and
    /// an administrator could approve a key nobody waited for).
    /// Told to the host as [`AuthorityEvent::KeyRequestCancelled`]; another relay's
    /// request is left as it is. One answered with a key just before: the relay dropped
    /// the key, so it goes to [`Authority::take_unclaimed_keys`] for the host to revoke.
    pub(super) fn key_withdrawn(&self, req: Option<&str>, conn: u64) {
        let req = text(req, 32);
        let gone = {
            let mut st = self.0.state.lock_ok();
            let id = st
                .key_requests
                .iter()
                .find(|(_, p)| p.conn == conn && p.req == req)
                .map(|(id, _)| *id);
            let gone = id.and_then(|id| st.key_requests.remove(&id).map(|_| id));
            // Already answered with a key, which the relay dropped: nobody's now.
            if gone.is_none()
                && let Some(i) = st
                    .key_answered
                    .iter()
                    .position(|(c, r, ..)| *c == conn && *r == req)
            {
                let (.., key, _) = st.key_answered.remove(i);
                st.key_unclaimed.push(key);
            }
            gone
        };
        if let Some(id) = gone {
            self.event(AuthorityEvent::KeyRequestCancelled(id));
        }
    }

    /// Ends the relayed requests older than their life: each relay is told (refused,
    /// "ended unanswered") and the host gets [`AuthorityEvent::KeyRequestCancelled`]. Call
    /// it now and then (the Site Manager's host does each second); listing or answering
    /// does it too.
    pub fn expire_key_requests(&self) {
        self.drop_key_requests(None);
    }

    /// How long a relayed request waits (tests: shorter than [`KEY_REQUEST_LIFE`]).
    #[doc(hidden)]
    pub fn set_key_request_life(&self, life: Duration) {
        self.0.state.lock_ok().key_request_life = life;
    }

    /// The machine on this trust connection leaves the site (its `idn_leave`): its
    /// certificate revoked, not listed, free to ask to join again (as a join withdrawn,
    /// §E.10.4).
    pub(super) fn left(&self, info: &CertInfo) -> io::Result<()> {
        let name = {
            let mut st = self.0.state.lock_ok();
            let Some(name) = st
                .devices
                .iter()
                .find(|r| r.info.fingerprint == info.fingerprint && r.info.serial == info.serial)
                .map(|r| r.info.name.clone())
            else {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "no such certificate",
                ));
            };
            // Every certificate it holds (its first one beside an admin one, or one from
            // an earlier join), as revoking does: the one it left with alone left the
            // other working, and the machine no longer listed to revoke it (bug run
            // 2026-11-25).
            for r in st
                .devices
                .iter_mut()
                .filter(|r| r.info.fingerprint == info.fingerprint && !r.revoked)
            {
                (r.revoked, r.withdrawn) = (true, true);
            }
            // A relay no more either: joining again doesn't bring it back unmarked (bug
            // run 2026-11-25).
            st.relays.remove(&info.fingerprint);
            st.pairing_codes.remove(&info.fingerprint);
            name
        };
        self.save()?;
        self.reissue_crl()?;
        self.event(AuthorityEvent::Left {
            name,
            fingerprint: info.fingerprint.clone(),
        });
        self.directory_changed(Vec::new(), 0);
        Ok(())
    }
}
