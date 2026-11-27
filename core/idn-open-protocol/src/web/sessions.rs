//! Logins: hand-off tokens, sessions, roles, the password gate, password hashes.

use super::*;

/// One-time tokens one program issues and another takes, with a key they share: the IDN
/// Node's panel opening IDN Data's. A token is good once, for 60 s:
/// `time.nonce.mac`, the MAC an HMAC-SHA256 of the time and nonce.
pub struct HandOff {
    key: ring::hmac::Key,
    raw: [u8; 32],
    used: Mutex<Vec<(String, u64)>>,
}

impl HandOff {
    const LIFE: u64 = 60;

    pub fn random() -> Self {
        Self::from_key(crate::random::secure_random::<32>())
    }

    pub fn from_key(raw: [u8; 32]) -> Self {
        HandOff {
            key: ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &raw),
            raw,
            used: Mutex::new(Vec::new()),
        }
    }

    /// The key as 64 hex digits (for the other program's environment), and back.
    pub fn key_hex(&self) -> String {
        idn_common::hex::hex(&self.raw)
    }

    pub fn from_hex(h: &str) -> Option<Self> {
        let raw = idn_common::hex::unhex(h.trim())?;
        Some(Self::from_key(raw.try_into().ok()?))
    }

    fn now() -> u64 {
        idn_common::epoch::secs()
    }

    fn mac(&self, t: u64, nonce: &str) -> ring::hmac::Tag {
        ring::hmac::sign(&self.key, format!("idn-handoff\0{t}.{nonce}").as_bytes())
    }

    pub fn issue(&self) -> String {
        let t = Self::now();
        let nonce = crate::random::token_hex(16);
        let mac = idn_common::hex::hex(self.mac(t, &nonce).as_ref());
        format!("{t}.{nonce}.{mac}")
    }

    /// True once for a token issued with this key within the last minute.
    pub fn take(&self, token: &str) -> bool {
        let mut parts = token.split('.');
        let (Some(t), Some(nonce), Some(mac), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return false;
        };
        let Ok(t) = t.parse::<u64>() else {
            return false;
        };
        let now = Self::now();
        if t > now + 5 || now.saturating_sub(t) > Self::LIFE || nonce.len() != 32 || mac.len() != 64
        {
            return false;
        }
        let Some(mac) = idn_common::hex::unhex(mac) else {
            return false;
        };
        if ring::hmac::verify(
            &self.key,
            format!("idn-handoff\0{t}.{nonce}").as_bytes(),
            &mac,
        )
        .is_err()
        {
            return false;
        }
        let mut used = self.used.lock_ok();
        used.retain(|(_, at)| now.saturating_sub(*at) <= Self::LIFE + 5);
        if used.iter().any(|(n, _)| n == nonce) {
            return false;
        }
        used.push((nonce.to_owned(), t));
        true
    }
}

/// Logged-in sessions, by random id.
pub struct Sessions {
    ttl: Duration,
    /// Each session's start and its user ("" where a panel has no users).
    ids: Mutex<HashMap<String, (Instant, String)>>,
}

impl Sessions {
    pub fn new(ttl: Duration) -> Self {
        Sessions {
            ttl,
            ids: Mutex::new(HashMap::new()),
        }
    }

    /// A new session id.
    pub fn create(&self) -> String {
        self.create_for("")
    }

    /// A new session id for `user`.
    pub fn create_for(&self, user: &str) -> String {
        let id = crate::random::token_hex(16);
        self.ids
            .lock_ok()
            .insert(id.clone(), (Instant::now(), user.to_owned()));
        id
    }

    pub fn valid(&self, id: Option<&str>) -> bool {
        self.user(id).is_some()
    }

    /// The user of a live session (`Some("")` for one made by [`create`](Self::create)).
    pub fn user(&self, id: Option<&str>) -> Option<String> {
        let id = id?;
        let mut s = self.ids.lock_ok();
        s.retain(|_, (at, _)| at.elapsed() < self.ttl);
        s.get(id).map(|(_, u)| u.clone())
    }

    /// Ends `user`'s sessions (their password or role changed, or they were removed).
    pub fn end_user(&self, user: &str) {
        self.ids.lock_ok().retain(|_, (_, u)| u != user);
    }

    pub fn end(&self, id: Option<&str>) {
        if let Some(id) = id {
            self.ids.lock_ok().remove(id);
        }
    }

    /// Ends every session (the password changed).
    pub fn end_all(&self) {
        self.ids.lock_ok().clear();
    }

    /// The `Set-Cookie` value for session `id` named `name` ([`set_cookie`]: `HttpOnly`,
    /// `SameSite=Strict`, kept as long as the session).
    pub fn cookie(&self, name: &str, id: &str, secure: bool) -> String {
        set_cookie(name, id)
            .max_age(self.ttl)
            .secure(secure)
            .to_string()
    }
}

/// Who may do what on a panel with users (the Industrial Site Manager, idn-industrial.md
/// §I.11.1; the media Site Manager, idn-node.md §N.31). Ordered: each role may do what
/// the ones before it may.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    /// Watches: status, monitoring, logs, the audit.
    Viewer,
    /// Watches and operates: routes, presets, routers, (Industrial) control and chat.
    Operator,
    /// Everything: machines, settings, users, backups, updates.
    Admin,
}

impl Role {
    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "viewer" => Some(Role::Viewer),
            "operator" => Some(Role::Operator),
            "admin" => Some(Role::Admin),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Operator => "operator",
            Role::Admin => "admin",
        }
    }
}

/// A password in front of a panel served on the network: login and logout requests,
/// sessions in a cookie, failed logins rate-limited per address, the password kept only
/// as an Argon2id hash. Used by the switcher's and the mixing desk's network panels.
/// With a [`TokenCheck`], site-signed tokens are taken too (idn-node.md §N.31.5), and a
/// gate may have tokens and no password (a machine in a site).
pub struct PasswordGate {
    hash: Option<String>,
    sessions: Arc<Sessions>,
    logins: Logins,
    /// The cookie's name, and the header every change must carry (`X-IDN-…: 1`).
    cookie: &'static str,
    header: &'static str,
    tokens: Option<Arc<TokenCheck>>,
}

impl PasswordGate {
    /// A gate for `password`, sessions lasting `ttl`.
    pub fn new(
        password: &str,
        ttl: Duration,
        cookie: &'static str,
        header: &'static str,
    ) -> io::Result<Self> {
        Ok(PasswordGate {
            hash: Some(hash_password(password)?),
            sessions: Arc::new(Sessions::new(ttl)),
            logins: Logins::default(),
            cookie,
            header,
            tokens: None,
        })
    }

    /// A gate that takes only site-signed tokens (no password; no logins).
    pub fn tokens_only(check: Arc<TokenCheck>, cookie: &'static str, header: &'static str) -> Self {
        PasswordGate {
            hash: None,
            sessions: Arc::new(Sessions::new(Duration::from_secs(1))),
            logins: Logins::default(),
            cookie,
            header,
            tokens: Some(check),
        }
    }

    /// This gate taking site-signed tokens too.
    pub fn with_tokens(mut self, check: Arc<TokenCheck>) -> Self {
        self.tokens = Some(check);
        self
    }

    /// Whether a password is set (logins taken).
    pub fn has_password(&self) -> bool {
        self.hash.is_some()
    }

    /// Whether `r` comes from a logged-in session, or carries a valid token.
    pub fn allowed(&self, r: &Request) -> bool {
        self.who(r).is_ok_and(|w| w.ready())
    }

    /// Who sent `r`: a valid token's holder, a logged-in session (`lan-password`), or a
    /// WebSocket that will give its token in its first message; else the answer (401 for
    /// a token refused, 403 for no login).
    pub fn who(&self, r: &Request) -> Result<Who, Response> {
        if let Some(t) = TokenCheck::carried(r) {
            let Some(check) = &self.tokens else {
                return Err(Response::json_err(
                    "401 Unauthorized",
                    "this engine takes no tokens here",
                ));
            };
            return check
                .check(&t)
                .map(|c| Who::token(r, c, check.clone()))
                .map_err(|e| auth::refused(&e));
        }
        if self.sessions.valid(r.cookie(self.cookie)) {
            return Ok(Who::lan_password(r));
        }
        match &self.tokens {
            Some(check) if auth::is_upgrade(r) => Ok(Who::pending(r, check.clone())),
            _ => Err(Response::json_err("403 Forbidden", "not logged in")),
        }
    }

    /// Whether `r`'s session is still valid, asked later (a WebSocket's, while it runs).
    pub fn still_allowed(&self, r: &Request) -> StillAllowed {
        let (sessions, cookie) = (
            self.sessions.clone(),
            r.cookie(self.cookie).map(str::to_owned),
        );
        Arc::new(move || sessions.valid(cookie.as_deref()))
    }

    /// What keeps `who`'s WebSocket going: its session, or its token (not expired, not
    /// withdrawn); one waiting for its token in its first message is ended by the channel.
    pub fn still_allowed_for(&self, r: &Request, who: &Who) -> StillAllowed {
        match who.via {
            Via::LanPassword => self.still_allowed(r),
            _ => {
                let who = who.clone();
                Arc::new(move || who.still_valid())
            }
        }
    }

    /// Answers `POST /api/v1/login` (`{"password": …}`) and `POST /api/v1/logout`;
    /// `None` for any other request. `log` hears logins and wrong passwords.
    pub fn handle(&self, r: &Request, log: &dyn Fn(&str)) -> Option<Response> {
        let err = Response::json_err;
        match (r.method.as_str(), r.path.as_str()) {
            ("POST", "/api/v1/login") => Some({
                if !r.may_change(self.header) {
                    err("403 Forbidden", &format!("missing {}: 1", self.header))
                } else if self.hash.is_none() {
                    err(
                        "403 Forbidden",
                        "no password here: use a token from the Site Manager",
                    )
                } else if !self.logins.attempt(r.peer) {
                    // Each try counts before its (slow) check, in the same step as the
                    // limit, so that tries at the same time can't all pass it (7
                    // overlapping wrong passwords were all let through); a right one
                    // clears the count.
                    err("429 Too Many Requests", "too many tries; wait a minute")
                } else {
                    let pw = serde_json::from_slice::<serde_json::Value>(&r.body)
                        .ok()
                        .and_then(|v| {
                            v.get("password")
                                .and_then(|p| p.as_str())
                                .map(str::to_owned)
                        })
                        .unwrap_or_default();
                    // (A few Argon2 checks at a time in the whole process: each takes ~19 MiB.)
                    if self.hash.as_deref().is_some_and(|h| {
                        PasswordChecks::shared().run_blocking(|| verify_password(h, &pw))
                    }) {
                        self.logins.succeeded(r.peer);
                        log(&format!("network login from {}", r.peer));
                        let sid = self.sessions.create();
                        Response::json(r#"{"ok":true}"#).with(
                            "Set-Cookie",
                            &self.sessions.cookie(self.cookie, &sid, r.tls),
                        )
                    } else {
                        log(&format!("wrong password from {}", r.peer));
                        err("403 Forbidden", "wrong password")
                    }
                }
            }),
            ("POST", "/api/v1/logout") if r.may_change(self.header) => {
                self.sessions.end(r.cookie(self.cookie));
                Some(Response::json(r#"{"ok":true}"#))
            }
            _ => None,
        }
    }
}

/// The file in the site authority's folder that holds the IDN Site Manager's site
/// password's hash.
pub const SITE_PASSWORD_FILE: &str = "panel-password";

/// Sets the IDN Site Manager's site password in the authority's folder `dir` (as its
/// Argon2id hash): by the Site Manager, and by an IDN Node that runs it (written as the
/// account the Site Manager runs as). At least 6 characters.
pub fn set_site_password(dir: &std::path::Path, password: &str) -> io::Result<()> {
    if password.chars().count() < 6 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "use at least 6 characters",
        ));
    }
    crate::trust::pki::write_private(
        &dir.join(SITE_PASSWORD_FILE),
        hash_password(password)?.as_bytes(),
    )
}

/// An Argon2id hash of a password (PHC string format).
pub fn hash_password(p: &str) -> io::Result<String> {
    use argon2::password_hash::PasswordHasher;
    argon2::Argon2::default()
        .hash_password_with_salt(p.as_bytes(), &crate::random::secure_random::<16>())
        .map(|h| h.to_string())
        .map_err(|e| io::Error::other(e.to_string()))
}

/// Whether `p` matches `hash` (from [`hash_password`]).
pub fn verify_password(hash: &str, p: &str) -> bool {
    use argon2::password_hash::PasswordVerifier;
    // (The parameters and salt are the hash's own: hashes made before argon2 0.6 check
    // the same.)
    argon2::Argon2::default()
        .verify_password(p.as_bytes(), hash)
        .is_ok()
}
