//! Key requests and pairing codes asked of the authority, their answers, leaving the site.

use super::*;

impl Enrollment {
    /// Relays a client's pairing to the site's Site Manager (idn-node.md §N.42.3): the
    /// client (`client`, its name for people) asks a key through this machine, with the
    /// PIN it typed, if any. The host running the Site Manager approves or refuses it; this
    /// machine only passes it on. A ticket for [`key_answer`](Self::key_answer).
    pub fn request_key(&self, client: &str, pin: Option<&str>) -> io::Result<u64> {
        if let Some(h) = &self.node {
            let mut e = Element::new("key_request").with_attr("client", client);
            if let Some(p) = pin {
                e = e.with_attr("pin", p);
            }
            let r = h.request(e, Duration::from_secs(10))?;
            return match r.attr("ticket").and_then(|t| t.parse().ok()) {
                Some(t) => Ok(t),
                None => Err(node_error(&r, "the IDN Node didn't take it")),
            };
        }
        if !matches!(self.state(), TrustState::Enrolled { connected: true }) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "not enrolled and connected to the site",
            ));
        }
        let ticket = {
            let mut g = self.key_answers.lock_ok();
            let t = g.0;
            g.0 += 1;
            // (At most 64 kept: the oldest go, with what was noted of them.)
            while g.1.len() >= 64 {
                let first = *g.1.keys().next().expect("not empty");
                g.1.remove(&first);
                self.key_code_wrong.lock_ok().remove(&first);
            }
            g.1.insert(t, None);
            t
        };
        let mut e = Element::new("idn_key_request")
            .with_attr("req", &ticket.to_string())
            .with_attr("client", client);
        if let Some(p) = pin {
            e = e.with_attr("pin", p);
        }
        // Not sent (the connection just ended): said so, nothing left waiting.
        if !self.trust_send(&e.to_xml()) {
            self.key_answers.lock_ok().1.remove(&ticket);
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "the connection to the site just ended: ask again",
            ));
        }
        Ok(ticket)
    }

    /// Takes back relayed request `ticket` ([`request_key`](Self::request_key)): gone from
    /// the site host's list (`KeyRequestCancelled`), its answer no longer kept here. For a
    /// client that typed a wrong code and asks again (the first
    /// stayed waiting 10 minutes, and could be approved for nobody). A request already
    /// answered is left as it is (`AlreadyExists`: collect it with `key_answer`); a key
    /// that arrives after it was taken back is revoked by the site's host.
    pub fn withdraw_key_request(&self, ticket: u64) -> io::Result<()> {
        if let Some(h) = &self.node {
            let r = h.request(
                Element::new("key_withdraw").with_attr("ticket", &ticket.to_string()),
                Duration::from_secs(10),
            )?;
            return if r.name == "ok" {
                Ok(())
            } else {
                Err(node_error(&r, "the IDN Node didn't take it back"))
            };
        }
        // Only one still waiting: an answer already here (a key, perhaps) is the
        // client's to collect (bug run 2026-11-24: it was dropped, the key nobody's).
        {
            let mut g = self.key_answers.lock_ok();
            match g.1.get(&ticket) {
                Some(None) => {
                    g.1.remove(&ticket);
                }
                Some(Some(_)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "already answered: collect it with key_answer",
                    ));
                }
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "no such pairing request",
                    ));
                }
            }
        }
        self.key_code_wrong.lock_ok().remove(&ticket);
        self.trust_send(
            &Element::new("idn_key_withdraw")
                .with_attr("req", &ticket.to_string())
                .to_xml(),
        );
        Ok(())
    }

    /// The pairing code this machine shows for its clients to type (idn-node.md §N.42.3):
    /// made by the site's Site Manager and sent here when asked; six
    /// digits, the same one until it is used, burnt or 5 minutes old. With when it ends
    /// (Unix seconds): ask again then, or after a pairing. A client's request with it
    /// ([`request_key`](Self::request_key), the code as the PIN) is checked by the site,
    /// which tells its host the code was right.
    pub fn pairing_code(&self, timeout: Duration) -> io::Result<(String, u64)> {
        self.pairing_code_left(timeout)
            .map(|(code, until, _)| (code, until))
    }

    /// [`pairing_code`](Self::pairing_code) with how long it has left, as the site counts
    /// it (a relay whose clock differs from the site's counts down from this).
    pub fn pairing_code_left(&self, timeout: Duration) -> io::Result<(String, u64, Duration)> {
        if let Some(h) = &self.node {
            let r = h.request(Element::new("pairing_code"), timeout)?;
            let left = r.attr("left").and_then(|l| l.parse().ok()).unwrap_or(0);
            return match (r.attr("code"), r.attr("until").and_then(|u| u.parse().ok())) {
                (Some(c), Some(u)) => Ok((c.to_owned(), u, Duration::from_secs(left))),
                _ => Err(node_error(&r, "the IDN Node gave no code")),
            };
        }
        if !matches!(self.state(), TrustState::Enrolled { connected: true }) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "not enrolled and connected to the site",
            ));
        }
        let req = {
            let mut g = self.key_answers.lock_ok();
            let t = g.0;
            g.0 += 1;
            t
        };
        if !self.trust_send(
            &Element::new("idn_pairing_code")
                .with_attr("req", &req.to_string())
                .to_xml(),
        ) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "the connection to the site just ended: ask again",
            ));
        }
        let end = Instant::now() + timeout;
        let (q, cv) = &self.pairing_codes;
        let mut q = q.lock_ok();
        loop {
            match q.remove(&req) {
                Some(Ok((code, until, left))) => {
                    return Ok((code, until, Duration::from_secs(left)));
                }
                Some(Err(why)) => return Err(io::Error::new(io::ErrorKind::PermissionDenied, why)),
                None => {}
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "no code from the site",
                ));
            }
            q = cv
                .wait_timeout(q, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    /// An `idn_pairing_code_result` from the authority: for its waiter.
    pub(crate) fn take_pairing_code(&self, m: &Element) {
        let Some(req) = m.attr("req").and_then(|r| r.parse::<u64>().ok()) else {
            return;
        };
        let answer = match (
            m.attr("code"),
            m.attr("until").and_then(|u| u.parse::<u64>().ok()),
        ) {
            (Some(code), Some(until)) => {
                let left = m
                    .attr("left")
                    .and_then(|l| l.parse::<u64>().ok())
                    .unwrap_or(0);
                Ok((code.to_owned(), until, left))
            }
            _ => Err(m
                .attr("error")
                .unwrap_or("the site gave no code")
                .chars()
                .take(200)
                .collect()),
        };
        let (q, cv) = &self.pairing_codes;
        let mut q = q.lock_ok();
        q.insert(req, answer);
        // (Answers no one waited for any more, the site slow: the oldest go, so a full
        // list never shuts out new ones.)
        while q.len() > 16 {
            let first = *q.keys().next().expect("not empty");
            q.remove(&first);
        }
        cv.notify_all();
    }

    /// The answer to relayed pairing `ticket` ([`request_key`](Self::request_key)): the key
    /// (approved) or why not; `None` while it waits (the host may take minutes: a person
    /// clicks Approve). Taken once.
    pub fn key_answer(&self, ticket: u64) -> Option<super::super::KeyGrant> {
        match self.key_status(ticket) {
            super::super::KeyStatus::Answered(g) => Some(g),
            super::super::KeyStatus::Waiting { .. } => None,
        }
    }

    /// Where relayed pairing `ticket` stands: waiting (and whether the site refused the
    /// code typed with it: the client may try again, the request still waits for a
    /// person), or answered (taken once, as [`key_answer`](Self::key_answer)).
    pub fn key_status(&self, ticket: u64) -> super::super::KeyStatus {
        use super::super::{KeyGrant, KeyStatus};
        if let Some(h) = &self.node {
            let r = match h.request(
                Element::new("key_answer").with_attr("ticket", &ticket.to_string()),
                Duration::from_secs(10),
            ) {
                Ok(r) => r,
                Err(_) => return KeyStatus::Waiting { code_wrong: false },
            };
            if r.name != "ok" {
                return KeyStatus::Answered(KeyGrant::refused(
                    r.attr("text")
                        .or(r.attr("error"))
                        .unwrap_or("the IDN Node refused"),
                ));
            }
            if r.attr("waiting") == Some("true") {
                return KeyStatus::Waiting {
                    code_wrong: r.attr("code") == Some("wrong"),
                };
            }
            let a = |k: &str| r.attr(k).unwrap_or("").to_owned();
            return KeyStatus::Answered(KeyGrant {
                approved: r.attr("approved") == Some("true"),
                id: a("key_id"),
                key: a("key"),
                scope: a("scope"),
                engine: a("engine"),
                error: a("error"),
            });
        }
        let mut g = self.key_answers.lock_ok();
        match g.1.get(&ticket) {
            Some(Some(_)) => {
                self.key_code_wrong.lock_ok().remove(&ticket);
                KeyStatus::Answered(g.1.remove(&ticket).flatten().expect("answered"))
            }
            Some(None) => KeyStatus::Waiting {
                code_wrong: self.key_code_wrong.lock_ok().contains(&ticket),
            },
            None => KeyStatus::Answered(KeyGrant::refused(
                "no such pairing (or its answer was taken)",
            )),
        }
    }

    /// An `idn_key_status` from the authority: the code typed with that ticket was wrong.
    pub(crate) fn take_key_status(&self, m: &Element) {
        if m.attr("code") == Some("wrong")
            && let Some(t) = m.attr("req").and_then(|r| r.parse::<u64>().ok())
        {
            let mut w = self.key_code_wrong.lock_ok();
            if w.len() < 64 {
                w.insert(t);
            }
        }
    }

    /// Every relayed pairing still waiting, refused with `why`.
    pub(crate) fn fail_waiting_keys(&self, why: &str) {
        let mut g = self.key_answers.lock_ok();
        for slot in g.1.values_mut().filter(|s| s.is_none()) {
            *slot = Some(super::super::KeyGrant::refused(why));
        }
    }

    /// An `idn_key_result` from the authority: kept for its ticket.
    pub(crate) fn take_key_result(&self, m: &Element) {
        let Some(t) = m.attr("req").and_then(|r| r.parse::<u64>().ok()) else {
            return;
        };
        let mut g = self.key_answers.lock_ok();
        if let Some(slot) = g.1.get_mut(&t) {
            *slot = Some(super::super::KeyGrant::from_element(m));
        }
    }

    /// Tells the site's authority this machine is leaving (idn-node.md §N.42): its
    /// certificate is revoked there and it is no longer listed (it may ask to join again).
    /// The caller then forgets its membership (the IDN Node's `leave`). Not through the
    /// IDN Node: a program the Node holds the membership for leaves with the machine.
    pub fn tell_leaving(&self) -> io::Result<()> {
        if self.node.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the IDN Node holds this machine's membership: leave the site through it",
            ));
        }
        // Not connected: nothing would reach the site (it would go on listing the machine
        // as offline); said, so the caller can wait for the connection or let an admin
        // remove it.
        if !matches!(self.state(), TrustState::Enrolled { connected: true }) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "not connected to the site: it can't be told (an admin can remove the machine there)",
            ));
        }
        self.trust_send(&Element::new("idn_leave").to_xml());
        Ok(())
    }

    /// A named API key `id` was used on this machine (idn-node.md §N.42): told to the
    /// site's authority (through the IDN Node for a program it holds the membership for).
    pub fn report_key_use(&self, id: &str) {
        match &self.node {
            Some(h) => h.send(Element::new("key_used").with_attr("key", id)),
            None => {
                self.trust_send(&Element::new("idn_key_used").with_attr("key", id).to_xml());
            }
        }
    }
}

/// The IDN Node's error answer as the error the call gives without a Node: its kind kept
/// (the Node's `kind`, else from its `code`), not all `Other` (bug run B: a non-relay's
/// `PermissionDenied`, an answered request's `AlreadyExists` were lost through the Node).
fn node_error(r: &Element, default: &str) -> io::Error {
    use io::ErrorKind as K;
    let kind = match r.attr("kind").or(r.attr("code")) {
        Some("PermissionDenied" | "denied" | "approval") => K::PermissionDenied,
        Some("AlreadyExists") => K::AlreadyExists,
        Some("NotFound") => K::NotFound,
        Some("NotConnected" | "state") => K::NotConnected,
        Some("TimedOut") => K::TimedOut,
        Some("InvalidInput" | "bad") => K::InvalidInput,
        _ => K::Other,
    };
    io::Error::new(kind, r.attr("text").unwrap_or(default).to_owned())
}
