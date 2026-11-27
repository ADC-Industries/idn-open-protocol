//! Who is asking an engine (idn-node.md §N.31.5): the same machine (open), a network
//! login with the engine's password, a site-signed token ([`crate::trust::token`]), or a
//! control path of the engine's own (OSC, a control surface, MQTT). Each request carries
//! a [`Who`]: what it may do (its scope) and how it is written in the audit.
//!
//! [`TokenCheck`] checks tokens for one engine on this machine with the site's CA key,
//! the site's id, this machine's fingerprint and the users' "not before" times, all from
//! the machine's site membership (an engine's [`crate::node::Node`] holds it, kept up to
//! date by the IDN Node).

use super::{Request, Response};
use crate::trust::token::{self, Claims, Scope, TokenError};
use idn_common::sync::LockOk as _;
use std::sync::Arc;

/// How a caller reached the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Via {
    /// This machine (127.0.0.1), or the program hosting the engine.
    Local,
    /// The network, logged in with the engine's password (a machine not in a site).
    LanPassword,
    /// The network, with a site-signed token.
    Token,
    /// OSC (the desk).
    Osc,
    /// A control surface (the desk's Mackie Control).
    Surface,
    /// MQTT (the switcher's commands and triggers).
    Mqtt,
    /// A WebSocket waiting for its first message, `{"token": …}`: nothing yet.
    Pending,
}

impl Via {
    pub fn as_str(self) -> &'static str {
        match self {
            Via::Local => "local",
            Via::LanPassword => "lan-password",
            Via::Token => "token",
            Via::Osc => "osc",
            Via::Surface => "surface",
            Via::Mqtt => "mqtt",
            Via::Pending => "pending",
        }
    }
}

/// Who made a request: the user written in the audit, their name, what they may do, and
/// from where.
#[derive(Clone)]
pub struct Who {
    /// As the audit says it: `local`, `lan-password`, the token's user, `osc ADDR`,
    /// `surface`, `mqtt`.
    pub user: String,
    /// The user's name for people (a token's `name`; else the same as `user`).
    pub name: String,
    /// What it may do.
    pub scope: Scope,
    /// The caller's address (empty for a surface or the hosting program).
    pub peer: String,
    /// The client's own name for itself (`X-IDN-Client`), if it gave one.
    pub client: String,
    /// How it reached the engine.
    pub via: Via,
    /// A token's claims (with `Via::Token`).
    pub token: Option<Claims>,
    /// What checks a token given later (a WebSocket's first message) or again (its life).
    check: Option<Arc<TokenCheck>>,
}

impl std::fmt::Debug for Who {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Who")
            .field("user", &self.user)
            .field("scope", &self.scope)
            .field("peer", &self.peer)
            .field("client", &self.client)
            .field("via", &self.via)
            .finish()
    }
}

/// The client's name for itself: `X-IDN-Client`, cut to 64 characters.
fn client_of(r: &Request) -> String {
    r.header("x-idn-client")
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect()
}

impl Who {
    fn new(user: &str, scope: Scope, peer: String, client: String, via: Via) -> Who {
        Who {
            user: user.to_owned(),
            name: user.to_owned(),
            scope,
            peer,
            client,
            via,
            token: None,
            check: None,
        }
    }

    /// A request on this machine (127.0.0.1): everything.
    pub fn local(r: &Request) -> Who {
        Who::new(
            "local",
            Scope::Control,
            r.peer.to_string(),
            client_of(r),
            Via::Local,
        )
    }

    /// The program hosting the engine (its typed calls), as `user` (`local` when it
    /// names none): everything.
    pub fn host(user: &str) -> Who {
        let user = if user.is_empty() { "local" } else { user };
        Who::new(
            user,
            Scope::Control,
            String::new(),
            String::new(),
            Via::Local,
        )
    }

    /// A network login with the engine's password: everything.
    pub fn lan_password(r: &Request) -> Who {
        Who::new(
            "lan-password",
            Scope::Control,
            r.peer.to_string(),
            client_of(r),
            Via::LanPassword,
        )
    }

    /// A user logged in with their own name and password (the Site Manager's users,
    /// idn-node.md §N.31; `user` empty: no one logged in): changes allowed when `may_change`
    /// (the engine checks each against the user's role).
    pub fn session(r: &Request, user: &str, may_change: bool) -> Who {
        Who::new(
            user,
            if may_change {
                Scope::Control
            } else {
                Scope::Read
            },
            r.peer.to_string(),
            client_of(r),
            Via::LanPassword,
        )
    }

    /// An OSC sender at `addr`: everything (OSC has no login; the desk's `--osc-allow`).
    pub fn osc(addr: impl std::fmt::Display) -> Who {
        let a = addr.to_string();
        Who::new(
            &format!("osc {a}"),
            Scope::Control,
            a,
            String::new(),
            Via::Osc,
        )
    }

    /// A control surface on this machine: everything.
    pub fn surface() -> Who {
        Who::new(
            "surface",
            Scope::Control,
            String::new(),
            String::new(),
            Via::Surface,
        )
    }

    /// A command or trigger from MQTT (the broker's `client`, when known).
    pub fn mqtt(client: &str) -> Who {
        Who::new(
            "mqtt",
            Scope::Control,
            String::new(),
            client.to_owned(),
            Via::Mqtt,
        )
    }

    /// A token's holder.
    pub fn token(r: &Request, claims: Claims, check: Arc<TokenCheck>) -> Who {
        let mut w = Who::new(
            &claims.sub,
            claims.scope.clone(),
            r.peer.to_string(),
            client_of(r),
            Via::Token,
        );
        w.name = if claims.name.is_empty() {
            claims.sub.clone()
        } else {
            claims.name.clone()
        };
        w.token = Some(claims);
        w.check = Some(check);
        w
    }

    /// A WebSocket that gives its token in its first message (browsers can't set its
    /// headers): nothing until then.
    pub fn pending(r: &Request, check: Arc<TokenCheck>) -> Who {
        let mut w = Who::new(
            "",
            Scope::Read,
            r.peer.to_string(),
            client_of(r),
            Via::Pending,
        );
        w.check = Some(check);
        w
    }

    /// Whether it may be served at all (not a WebSocket still waiting for its token).
    pub fn ready(&self) -> bool {
        self.via != Via::Pending
    }

    /// Whether it may change things: `Err` with why not ("read-only", "scope not
    /// supported yet" for one bus or IEM mix until those are built, "not logged in").
    pub fn may_change(&self) -> Result<(), &'static str> {
        if self.via == Via::Pending {
            return Err("not logged in: send {\"token\": …} first");
        }
        match self.scope {
            Scope::Control => Ok(()),
            Scope::Read => Err("read-only"),
            Scope::Bus(_) | Scope::Iem(_) => {
                Err("read-only: this token's scope (one bus or IEM mix) is not supported yet")
            }
        }
    }

    /// The refusal of a change it may not make (`403`, or `401` before a token), if so.
    pub fn refusal(&self) -> Option<Response> {
        let e = self.may_change().err()?;
        let status = if self.via == Via::Pending {
            "401 Unauthorized"
        } else {
            "403 Forbidden"
        };
        Some(Response::json_err(status, e))
    }

    /// Whether it still holds: a token not expired, its user's not withdrawn.
    pub fn still_valid(&self) -> bool {
        match (&self.token, &self.check) {
            (Some(c), Some(check)) => check.recheck(c),
            _ => true,
        }
    }

    /// A token given now (a WebSocket's `{"token": …}`): checked, and this caller is its
    /// holder from now on (peer and client kept).
    pub fn accept_token(&mut self, tok: &str) -> Result<(), TokenError> {
        let check = self.check.clone().ok_or(TokenError::NoSite)?;
        let c = check.check(tok)?;
        self.user = c.sub.clone();
        self.name = if c.name.is_empty() {
            c.sub.clone()
        } else {
            c.name.clone()
        };
        self.scope = c.scope.clone();
        self.via = Via::Token;
        self.token = Some(c);
        Ok(())
    }
}

/// What a token is checked with on this machine: the site CA's public key, the site's id,
/// this machine's fingerprint, the site's options (the users' "not before").
#[derive(Clone)]
pub struct SiteKeys {
    /// The site CA's Ed25519 public key.
    pub ca_public: [u8; 32],
    /// The site's id (`x_idn_ca`).
    pub site: String,
    /// This machine's fingerprint.
    pub machine: String,
    /// The site's options (the users' "not before").
    pub options: Arc<crate::trust::SiteOptions>,
}

impl SiteKeys {
    /// A site membership's, as it holds now: the CA from its certificate, the options as
    /// the authority last sent them. `None` while it isn't enrolled.
    pub fn of(e: &crate::trust::Enrollment) -> Option<SiteKeys> {
        let c = e.creds()?;
        let spki = c.anchor.ca_der_spki().ok()?;
        Some(SiteKeys {
            ca_public: crate::trust::pki::public_from_spki(&spki)?,
            site: c.anchor.ca_id().to_owned(),
            machine: e.fingerprint(),
            options: e.options(),
        })
    }
}

type KeysFn = dyn Fn() -> Option<SiteKeys> + Send + Sync;

/// Checks tokens for one engine on this machine.
pub struct TokenCheck {
    engine: String,
    /// The keys now (`None`: not in a site).
    keys: Box<KeysFn>,
    /// Where a named API key's use is told (its site's authority, through the
    /// membership; idn-node.md §N.42 `last_used`), and when each key was last told.
    uses: Option<Arc<crate::trust::Enrollment>>,
    told: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
}

impl std::fmt::Debug for TokenCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TokenCheck({})", self.engine)
    }
}

impl TokenCheck {
    /// Checks tokens for `engine` (`desk`, `switcher`, `data`, `node`, …) with what `keys`
    /// gives at each check.
    pub fn new(
        engine: &str,
        keys: impl Fn() -> Option<SiteKeys> + Send + Sync + 'static,
    ) -> Arc<TokenCheck> {
        Arc::new(TokenCheck {
            engine: engine.to_owned(),
            keys: Box::new(keys),
            uses: None,
            told: Default::default(),
        })
    }

    /// Checks tokens for `engine` with the machine's site membership (an engine's
    /// `Node::enrollment()`, or the Node's own): the CA from its certificate, the "not
    /// before" times from the site's options, both as the IDN Node keeps them now.
    pub fn from_enrollment(engine: &str, e: Arc<crate::trust::Enrollment>) -> Arc<TokenCheck> {
        let keys = e.clone();
        Arc::new(TokenCheck {
            engine: engine.to_owned(),
            keys: Box::new(move || SiteKeys::of(&keys)),
            uses: Some(e),
            told: Default::default(),
        })
    }

    /// The engine it checks for.
    pub fn engine(&self) -> &str {
        &self.engine
    }

    /// Whether tokens can be checked now (the machine is in a site).
    pub fn available(&self) -> bool {
        (self.keys)().is_some()
    }

    /// Checks `tok` now: its claims, or why not.
    pub fn check(&self, tok: &str) -> Result<Claims, TokenError> {
        let c = self.verify(tok)?;
        // A named API key used: told to the site, at most once a minute a key.
        if c.key
            && let (Some(e), Some(id)) = (&self.uses, c.sub.strip_prefix("key:"))
        {
            let mut told = self.told.lock_ok();
            let due = told
                .get(id)
                .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(60));
            if due {
                told.insert(id.to_owned(), std::time::Instant::now());
                e.report_key_use(id);
            }
        }
        Ok(c)
    }

    fn verify(&self, tok: &str) -> Result<Claims, TokenError> {
        let k = (self.keys)().ok_or(TokenError::NoSite)?;
        token::verify(
            tok,
            &token::Expect {
                ca_public: &k.ca_public,
                site: &k.site,
                engine: &self.engine,
                machine: &k.machine,
                now: idn_common::epoch::secs(),
            },
            |user| token::not_before(&k.options, &k.machine, user),
        )
    }

    /// Whether checked claims still hold: not expired, the user's tokens not withdrawn
    /// since, the machine still in the same site.
    pub fn recheck(&self, c: &Claims) -> bool {
        let Some(k) = (self.keys)() else {
            return false;
        };
        k.site == c.site
            && c.exp + token::SKEW >= idn_common::epoch::secs()
            && token::not_before(&k.options, &k.machine, &c.sub).is_none_or(|nb| c.iat >= nb)
    }

    /// The token a request carries: `Authorization: Bearer …` ([`super::bearer`]), or
    /// `?token=…` on a WebSocket's upgrade (a browser can't set its headers; elsewhere a
    /// token in an address ends up in logs).
    pub fn carried(r: &Request) -> Option<String> {
        if let Some(t) = super::bearer(r) {
            return Some(t.to_owned());
        }
        if is_upgrade(r) {
            return r.param("token").map(str::to_owned);
        }
        None
    }

    /// The caller of a request carrying a token: `None` when it carries none.
    pub fn who(self: &Arc<Self>, r: &Request) -> Option<Result<Who, TokenError>> {
        let t = Self::carried(r)?;
        Some(self.check(&t).map(|c| Who::token(r, c, self.clone())))
    }
}

/// A WebSocket's upgrade request.
pub fn is_upgrade(r: &Request) -> bool {
    r.method == "GET"
        && r.header("upgrade")
            .is_some_and(|u| u.eq_ignore_ascii_case("websocket"))
}

/// A token refused, as answered: 401 with why.
pub fn refused(e: &TokenError) -> Response {
    Response::json_err("401 Unauthorized", &e.to_string())
}

/// One push channel's caller ([`super::events`]): its [`Who`], taking a first
/// `{"token": …}` message when it came without one, and ended when its token no longer
/// holds.
pub struct Channel {
    who: std::cell::RefCell<Who>,
    opened: std::time::Instant,
    checked: std::cell::Cell<std::time::Instant>,
}

/// How long a channel without a token waits for its `{"token": …}` message.
pub const TOKEN_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

impl Channel {
    pub fn new(who: Who) -> Channel {
        let now = std::time::Instant::now();
        Channel {
            who: std::cell::RefCell::new(who),
            opened: now,
            checked: std::cell::Cell::new(now),
        }
    }

    /// The caller now.
    pub fn who(&self) -> Who {
        self.who.borrow().clone()
    }

    /// Whether anything may be sent yet (a token given, or none needed).
    pub fn ready(&self) -> bool {
        self.who.borrow().ready()
    }

    /// Whether the channel is to end: no token within [`TOKEN_WAIT`], or its token no
    /// longer holds (looked at once a second).
    pub fn ended(&self) -> bool {
        let w = self.who.borrow();
        if !w.ready() {
            return self.opened.elapsed() >= TOKEN_WAIT;
        }
        if self.checked.get().elapsed() >= std::time::Duration::from_secs(1) {
            self.checked.set(std::time::Instant::now());
            return !w.still_valid();
        }
        false
    }

    /// A message the channel itself answers: `{"token": …}` (`{"type": "token", "ok",
    /// "user", "scope"}` or its error), and anything sent before a token (refused).
    /// `None`: the engine's own message.
    pub fn intercept(&self, v: &serde_json::Value) -> Option<serde_json::Value> {
        use serde_json::json;
        if let Some(t) = v.get("token").and_then(|t| t.as_str()) {
            let mut w = self.who.borrow_mut();
            if w.check.is_none() {
                return Some(
                    json!({"type": "token", "ok": false, "error": "this channel takes no token"}),
                );
            }
            return Some(match w.accept_token(t) {
                Ok(()) => {
                    json!({"type": "token", "ok": true, "user": w.user, "scope": w.scope.as_string()})
                }
                Err(e) => json!({"type": "token", "ok": false, "error": e.to_string()}),
            });
        }
        if !self.ready() {
            let mut r =
                json!({"type": "error", "error": "not logged in: send {\"token\": …} first"});
            if let Some(id) = v.get("id").filter(|i| !i.is_null()) {
                r["type"] = json!("reply");
                r["ok"] = json!(false);
                r["id"] = id.clone();
            }
            return Some(r);
        }
        None
    }

    /// Whether its caller may change things (see [`Who::may_change`]).
    pub fn may_change(&self) -> Result<(), String> {
        self.who.borrow().may_change().map_err(str::to_owned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trust::pki::KeyFile;

    fn req(headers: &[(&str, &str)], query: &str) -> Request {
        Request {
            method: "GET".into(),
            path: "/api/v1/state".into(),
            query: query.into(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: Vec::new(),
            peer: "10.0.0.7".parse().unwrap(),
            tls: true,
        }
    }

    #[test]
    fn a_token_makes_a_caller_with_its_scope() {
        let key = KeyFile::generate().unwrap();
        let site = key.fingerprint()[..16].to_owned();
        let (public, s2) = (key.public, site.clone());
        let check = TokenCheck::new("switcher", move || {
            Some(SiteKeys {
                ca_public: public,
                site: s2.clone(),
                machine: "fp1".into(),
                options: Default::default(),
            })
        });
        let c = token::Claims::new(
            &site,
            "alice",
            "Alice",
            "switcher",
            "fp1",
            Scope::Read,
            idn_common::epoch::secs(),
            600,
        )
        .unwrap();
        let tok = token::sign(&c, &key).unwrap();
        let bearer = format!("Bearer {tok}");
        let w = check
            .who(&req(
                &[("authorization", &bearer), ("x-idn-client", "ExampleApp")],
                "",
            ))
            .unwrap()
            .unwrap();
        assert_eq!(
            (w.user.as_str(), w.via, w.peer.as_str(), w.client.as_str()),
            ("alice", Via::Token, "10.0.0.7", "ExampleApp")
        );
        assert_eq!(w.may_change(), Err("read-only"));
        assert!(w.refusal().unwrap().status.starts_with("403"));
        assert!(w.still_valid());
        // ?token= only on an upgrade.
        assert!(check.who(&req(&[], &format!("token={tok}"))).is_none());
        let up = req(&[("upgrade", "websocket")], &format!("a=1&token={tok}"));
        assert!(check.who(&up).unwrap().is_ok());
        // A pending channel takes its token from its first message.
        let ch = Channel::new(Who::pending(&up, check.clone()));
        assert!(!ch.ready());
        let early = ch
            .intercept(&serde_json::json!({"id": 1, "cmd": "cut"}))
            .unwrap();
        assert_eq!(early["ok"], false);
        let bad = ch
            .intercept(&serde_json::json!({"token": "idn1.x.y"}))
            .unwrap();
        assert_eq!(bad["ok"], false);
        let good = ch.intercept(&serde_json::json!({"token": tok})).unwrap();
        assert_eq!(good["ok"], true, "{good}");
        assert!(ch.ready());
        assert_eq!(ch.may_change(), Err("read-only".to_owned()));
        assert!(ch.intercept(&serde_json::json!({"cmd": "cut"})).is_none());
        // Bus and IEM scopes: no changes yet.
        assert!(Who::host("").may_change().is_ok());
        let mut b = Who::host("x");
        b.scope = Scope::Bus("aux:5".into());
        assert!(b.may_change().unwrap_err().contains("not supported yet"));
        // Not in a site: a token refused as such.
        let none = TokenCheck::new("desk", || None);
        assert_eq!(
            none.who(&req(&[("authorization", &bearer)], ""))
                .unwrap()
                .unwrap_err(),
            TokenError::NoSite
        );
    }
}
