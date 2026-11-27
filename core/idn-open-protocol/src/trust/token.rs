//! Site-signed access tokens (spec/extensions/idn-node.md §N.31.5): what a caller on the
//! network shows an engine (the desk, the switcher, IDN Data, the Node's panel) in place
//! of a password. The Site Manager signs them with the site CA's key (the one that
//! issues the machines' certificates); each engine checks them with the CA's public key,
//! which its IDN Node already holds.
//!
//! A token is `idn1.<base64url(claims)>.<base64url(signature)>`: the claims a JSON
//! object, the signature Ed25519 over `"IDN token v1"` followed by the claims' bytes.
//!
//! ```json
//! {"v": 1, "site": "<x_idn_ca>", "sub": "alice", "name": "Alice",
//!  "aud": {"engine": "desk", "machine": "<fingerprint>|*"},
//!  "scope": "read", "iat": 1763450000, "exp": 1763493200, "jti": "<16 hex>"}
//! ```
//!
//! A token lives 12 hours at most. A user's tokens issued before their "not before"
//! (the site option `tokens.not_before.<id>`, [`not_before_key`]) are refused: removing a
//! user, or changing their role or password, ends their tokens at once.

use super::options::SiteOptions;
use super::pki::KeyFile;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fmt;
use std::io;

/// What every token starts with (the format's version).
pub const PREFIX: &str = "idn1.";
/// What the signature covers before the claims' bytes.
pub const CONTEXT: &[u8] = b"IDN token v1";
/// The longest a token may live: 12 hours.
pub const MAX_LIFE: u64 = 12 * 3600;
/// The longest a named API key may live (long-lived keys for Stream
/// Deck, Companion, a box's web page; revoked one by one): 10 years.
pub const MAX_KEY_LIFE: u64 = 10 * 365 * 24 * 3600;
/// Clock difference allowed between the Site Manager and an engine (the site clock keeps
/// machines closer than this).
pub const SKEW: u64 = 60;
/// The engines a token can be for (and a host program's own API: [`APP_PREFIX`]).
pub const ENGINES: &[&str] = &["desk", "switcher", "display", "data", "node"];
/// The audience of a program's own API (`app:myapp`): tokens the Site Manager signs for
/// a program hosting it, which checks them itself (`idn_site_manager::host::Handle::
/// verify_token`). The name after it: 1 to 32 of `a-z`, `0-9`, `-`, `_`, `.`.
pub const APP_PREFIX: &str = "app:";

/// Whether a token may be for `engine`: one of [`ENGINES`], or `app:<name>`.
pub fn engine_ok(engine: &str) -> bool {
    ENGINES.contains(&engine)
        || engine.strip_prefix(APP_PREFIX).is_some_and(|n| {
            !n.is_empty()
                && n.len() <= 32
                && n.chars().all(|c| {
                    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.')
                })
        })
}
/// The site option prefix of the users' "not before" times (`tokens.not_before.<id>`).
pub const NOT_BEFORE_PREFIX: &str = "tokens.not_before.";
/// The most a token may be (claims of a few hundred bytes): longer is refused unread.
const MAX_LEN: usize = 4096;

/// What a token allows.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Scope {
    /// Reads only: state, meters, events, the audit. Every change is refused.
    Read,
    /// Everything the engine's API offers.
    Control,
    /// One bus of the desk (`bus:main`, `bus:aux:5`, `bus:matrix:2`, `bus:stream`): the
    /// channels' sends into it and its own master and processing, with the state, meters
    /// and limits filtered to it; enforced by the desk (idn-mixer.md §M.25). Other
    /// engines take it as [`Scope::Read`] and refuse every change.
    Bus(String),
    /// One IEM mix (`iem:<mix id>`): that mix's changes, its IEM channels and the desk's
    /// IEM flow and talkback into it; enforced by the desk (idn-mixer.md §M.26). Other
    /// engines take it as [`Scope::Read`] and refuse every change.
    Iem(String),
}

impl Scope {
    /// `read`, `control`, `bus:<bus>` or `iem:<mix>` (a bus or mix of up to 64
    /// characters: letters, digits, `:`, `-`, `_`, `.`).
    pub fn parse(s: &str) -> Option<Scope> {
        let ok = |x: &str| {
            !x.is_empty()
                && x.len() <= 64
                && x.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_' | '.'))
        };
        match s {
            "read" => Some(Scope::Read),
            "control" => Some(Scope::Control),
            _ => {
                if let Some(b) = s.strip_prefix("bus:").filter(|b| ok(b)) {
                    Some(Scope::Bus(b.to_owned()))
                } else {
                    s.strip_prefix("iem:")
                        .filter(|m| ok(m))
                        .map(|m| Scope::Iem(m.to_owned()))
                }
            }
        }
    }

    /// As written in a token.
    pub fn as_string(&self) -> String {
        match self {
            Scope::Read => "read".into(),
            Scope::Control => "control".into(),
            Scope::Bus(b) => format!("bus:{b}"),
            Scope::Iem(m) => format!("iem:{m}"),
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_string())
    }
}

/// A token's claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claims {
    /// The site: the first 16 hex digits of the CA's fingerprint (`x_idn_ca`).
    pub site: String,
    /// The user (the Site Manager's user name; a host program's own user for its own tokens).
    pub sub: String,
    /// The user's name for people (may be the same).
    pub name: String,
    /// The engine it is for ([`ENGINES`]).
    pub engine: String,
    /// The machine it is for: its fingerprint, or `*` for any of the site's.
    pub machine: String,
    /// What it allows.
    pub scope: Scope,
    /// Issued, Unix seconds.
    pub iat: u64,
    /// Expires, Unix seconds.
    pub exp: u64,
    /// Its id (16 hex digits), for the audit.
    pub jti: String,
    /// A named API key (its `sub` `key:<id>`): may live up to [`MAX_KEY_LIFE`], withdrawn
    /// by its own not-before.
    pub key: bool,
}

impl Claims {
    /// The longest these claims may live.
    pub fn max_life(&self) -> u64 {
        if self.key { MAX_KEY_LIFE } else { MAX_LIFE }
    }

    /// A named API key's claims: `sub` `key:<id>`, living [`MAX_KEY_LIFE`].
    pub fn new_key(
        site: &str,
        id: &str,
        name: &str,
        engine: &str,
        machine: &str,
        scope: Scope,
        iat: u64,
    ) -> io::Result<Claims> {
        let mut c = Claims::new(
            site,
            &format!("key:{id}"),
            name,
            engine,
            machine,
            scope,
            iat,
            1,
        )?;
        c.exp = iat + MAX_KEY_LIFE;
        c.key = true;
        Ok(c)
    }
}

/// Why a token was refused (each said to the caller as a 401).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenError {
    /// Not a token of this format.
    Malformed,
    /// Its signature isn't the site's.
    Forged,
    /// A version this engine doesn't know.
    Version,
    /// For another site.
    WrongSite,
    /// For another engine.
    WrongEngine,
    /// For another machine.
    WrongMachine,
    /// Issued in the future (beyond the allowed clock difference).
    NotYet,
    /// Past its expiry.
    Expired,
    /// Living longer than 12 hours.
    TooLong,
    /// Issued before the user's "not before" (removed, or their role or password changed).
    Revoked,
    /// This machine isn't in a site (no CA to check with).
    NoSite,
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TokenError::Malformed => "not an IDN token",
            TokenError::Forged => "the token isn't signed by this site",
            TokenError::Version => "a token version this engine doesn't know",
            TokenError::WrongSite => "the token is for another site",
            TokenError::WrongEngine => "the token is for another engine",
            TokenError::WrongMachine => "the token is for another machine",
            TokenError::NotYet => "the token isn't valid yet (check the clocks)",
            TokenError::Expired => "the token expired",
            TokenError::TooLong => "the token lives longer than 12 hours",
            TokenError::Revoked => "the token was withdrawn (the user changed or was removed)",
            TokenError::NoSite => "this machine isn't in a site: no tokens",
        })
    }
}

impl std::error::Error for TokenError {}

impl Claims {
    /// Claims for `sub` (`name`), for `engine` on `machine` (`*`: any), with `scope`,
    /// from `iat` for `ttl` seconds (at most 12 hours), with a new random id.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        site: &str,
        sub: &str,
        name: &str,
        engine: &str,
        machine: &str,
        scope: Scope,
        iat: u64,
        ttl: u64,
    ) -> io::Result<Claims> {
        use idn_common::err::invalid_input as bad;
        if ttl == 0 || ttl > MAX_LIFE {
            return Err(bad("a token lives 1 s to 12 hours"));
        }
        if !engine_ok(engine) {
            return Err(bad(
                "engine: desk, switcher, display, data, node or app:<name>",
            ));
        }
        if machine.is_empty() || machine.len() > 64 {
            return Err(bad("machine: a fingerprint, or * for any"));
        }
        if sub.is_empty() || sub.chars().count() > 64 || sub.chars().any(char::is_control) {
            return Err(bad("a user name of up to 64 characters"));
        }
        Ok(Claims {
            site: site.to_owned(),
            sub: sub.to_owned(),
            name: name.chars().filter(|c| !c.is_control()).take(64).collect(),
            engine: engine.to_owned(),
            machine: machine.to_owned(),
            scope,
            iat,
            exp: iat + ttl,
            jti: idn_common::hex::hex(&crate::random::secure_random::<8>()),
            key: false,
        })
    }

    /// The claims as JSON.
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "v": 1,
            "site": self.site,
            "sub": self.sub,
            "name": self.name,
            "aud": {"engine": self.engine, "machine": self.machine},
            "scope": self.scope.as_string(),
            "iat": self.iat,
            "exp": self.exp,
            "jti": self.jti,
        });
        if self.key {
            v["key"] = json!(true);
        }
        v
    }

    fn from_json(v: &Value) -> Result<Claims, TokenError> {
        if v["v"].as_u64() != Some(1) {
            return Err(if v["v"].is_u64() {
                TokenError::Version
            } else {
                TokenError::Malformed
            });
        }
        let s = |x: &Value| x.as_str().map(str::to_owned).ok_or(TokenError::Malformed);
        Ok(Claims {
            site: s(&v["site"])?,
            sub: s(&v["sub"])?,
            name: v["name"].as_str().unwrap_or("").to_owned(),
            engine: s(&v["aud"]["engine"])?,
            machine: s(&v["aud"]["machine"])?,
            scope: v["scope"]
                .as_str()
                .and_then(Scope::parse)
                .ok_or(TokenError::Malformed)?,
            iat: v["iat"].as_u64().ok_or(TokenError::Malformed)?,
            exp: v["exp"].as_u64().ok_or(TokenError::Malformed)?,
            jti: v["jti"].as_str().unwrap_or("").to_owned(),
            // (Only a `key:` subject may be a key.)
            key: v["key"] == true && v["sub"].as_str().is_some_and(|s| s.starts_with("key:")),
        })
    }
}

/// Base64url without padding (RFC 4648 §5).
fn b64url(b: &[u8]) -> String {
    idn_common::text::base64(b)
        .trim_end_matches('=')
        .chars()
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            c => c,
        })
        .collect()
}

fn unb64url(s: &str) -> Option<Vec<u8>> {
    if s.bytes().any(|c| matches!(c, b'+' | b'/' | b'=')) {
        return None;
    }
    let mut t: String = s
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            c => c,
        })
        .collect();
    while !t.len().is_multiple_of(4) {
        t.push('=');
    }
    idn_common::text::unbase64(t)
}

/// The bytes signed: [`CONTEXT`] and the claims' bytes.
fn signed_bytes(claims: &[u8]) -> Vec<u8> {
    let mut m = CONTEXT.to_vec();
    m.extend_from_slice(claims);
    m
}

/// Signs `claims` with the site CA's key `key` (here, or held by another process): the
/// token. Refused for claims living longer than 12 hours.
pub fn sign(claims: &Claims, key: &KeyFile) -> io::Result<String> {
    if claims.exp <= claims.iat || claims.exp - claims.iat > claims.max_life() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a token lives 1 s to 12 hours",
        ));
    }
    let body = claims.to_json().to_string().into_bytes();
    let sig = key.sign(&signed_bytes(&body))?;
    Ok(format!("{PREFIX}{}.{}", b64url(&body), b64url(&sig)))
}

/// A token's claims, read without checking anything (to show what a token says).
pub fn peek(token: &str) -> Result<Claims, TokenError> {
    split(token).and_then(|(body, _)| {
        let v: Value = serde_json::from_slice(&body).map_err(|_| TokenError::Malformed)?;
        Claims::from_json(&v)
    })
}

/// The audience engine a token names (`app:myapp`, `desk`), read WITHOUT checking it: only
/// to pick what checks it ([`verify`] then decides).
pub fn engine_of(token: &str) -> Option<String> {
    let (body, _) = split(token.trim()).ok()?;
    let v: Value = serde_json::from_slice(&body).ok()?;
    v["aud"]["engine"].as_str().map(str::to_owned)
}

fn split(token: &str) -> Result<(Vec<u8>, Vec<u8>), TokenError> {
    if token.len() > MAX_LEN {
        return Err(TokenError::Malformed);
    }
    let rest = token.strip_prefix(PREFIX).ok_or(TokenError::Malformed)?;
    let (body, sig) = rest.split_once('.').ok_or(TokenError::Malformed)?;
    Ok((
        unb64url(body).ok_or(TokenError::Malformed)?,
        unb64url(sig).ok_or(TokenError::Malformed)?,
    ))
}

/// What an engine checks a token against.
#[derive(Debug, Clone)]
pub struct Expect<'a> {
    /// The site CA's Ed25519 public key.
    pub ca_public: &'a [u8; 32],
    /// This machine's site (`x_idn_ca`).
    pub site: &'a str,
    /// This engine ([`ENGINES`]).
    pub engine: &'a str,
    /// This machine's fingerprint.
    pub machine: &'a str,
    /// Unix seconds now.
    pub now: u64,
}

/// Checks `token`: the site's signature, the version, the site, the engine, the machine
/// (this one or `*`), the time (with [`SKEW`] either way) and the life; then the user's
/// "not before" from `not_before` (given the user). Its claims, or why not.
pub fn verify(
    token: &str,
    want: &Expect,
    not_before: impl Fn(&str) -> Option<u64>,
) -> Result<Claims, TokenError> {
    let (body, sig) = split(token.trim())?;
    let key = ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, want.ca_public);
    key.verify(&signed_bytes(&body), &sig)
        .map_err(|_| TokenError::Forged)?;
    let v: Value = serde_json::from_slice(&body).map_err(|_| TokenError::Malformed)?;
    let c = Claims::from_json(&v)?;
    if c.site != want.site {
        return Err(TokenError::WrongSite);
    }
    if c.engine != want.engine {
        return Err(TokenError::WrongEngine);
    }
    if c.machine != "*" && c.machine != want.machine {
        return Err(TokenError::WrongMachine);
    }
    if c.exp <= c.iat || c.exp - c.iat > c.max_life() {
        return Err(TokenError::TooLong);
    }
    if c.iat > want.now + SKEW {
        return Err(TokenError::NotYet);
    }
    if c.exp + SKEW < want.now {
        return Err(TokenError::Expired);
    }
    if not_before(&c.sub).is_some_and(|nb| c.iat < nb) {
        return Err(TokenError::Revoked);
    }
    Ok(c)
}

/// The site option holding `user`'s "not before" (`tokens.not_before.<id>`, the id the
/// first 16 hex digits of the SHA-256 of the user name: option keys are lower-case).
pub fn not_before_key(user: &str) -> String {
    let h = Sha256::digest(user.as_bytes());
    format!("{NOT_BEFORE_PREFIX}{}", idn_common::hex::hex(&h[..8]))
}

/// `user`'s "not before" in the site's options (as resolved for machine `fp`), if set.
pub fn not_before(opts: &SiteOptions, fp: &str, user: &str) -> Option<u64> {
    opts.for_device(fp)
        .get(&not_before_key(user))
        .and_then(|v| v.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Site {
        key: KeyFile,
        id: String,
    }

    fn site() -> Site {
        let key = KeyFile::generate().unwrap();
        let id = key.fingerprint()[..16].to_owned();
        Site { key, id }
    }

    fn claims(s: &Site, engine: &str, machine: &str, scope: Scope, iat: u64) -> Claims {
        Claims::new(&s.id, "alice", "Alice", engine, machine, scope, iat, 3600).unwrap()
    }

    fn expect<'a>(s: &'a Site, engine: &'a str, machine: &'a str, now: u64) -> Expect<'a> {
        Expect {
            ca_public: &s.key.public,
            site: &s.id,
            engine,
            machine,
            now,
        }
    }

    /// A named API key: lives for years, is checked as a key, stops
    /// at its own not-before; a user's token can't claim a key's life.
    #[test]
    fn api_keys_live_long_and_stop_one_by_one() {
        let s = site();
        let t0 = 1_763_450_000;
        let none = |_: &str| None;
        let c = Claims::new_key(
            &s.id,
            "ab12",
            "Stream Deck",
            "switcher",
            "*",
            Scope::Control,
            t0,
        )
        .unwrap();
        assert_eq!((c.sub.as_str(), c.key), ("key:ab12", true));
        let tok = sign(&c, &s.key).unwrap();
        let year = 365 * 24 * 3600;
        let got = verify(&tok, &expect(&s, "switcher", "fp1", t0 + 3 * year), none).unwrap();
        assert!(got.key);
        // Its own not-before stops it; another key's doesn't.
        let nb = |sub: &str| (sub == "key:ab12").then_some(t0 + 10);
        assert_eq!(
            verify(&tok, &expect(&s, "switcher", "fp1", t0 + 20), nb).unwrap_err(),
            TokenError::Revoked
        );
        // A user's token stretched past 12 hours, or one saying "key" without a key's
        // subject: refused.
        let mut long = claims(&s, "switcher", "*", Scope::Control, t0);
        long.exp = t0 + MAX_LIFE + 1;
        assert!(sign(&long, &s.key).is_err());
        let mut fake = claims(&s, "switcher", "*", Scope::Control, t0);
        fake.key = true;
        fake.exp = t0 + year;
        let forged = sign(&fake, &s.key).unwrap();
        assert_eq!(
            verify(&forged, &expect(&s, "switcher", "fp1", t0 + 10), none).unwrap_err(),
            TokenError::TooLong
        );
    }

    /// A token made and checked; each wrong thing refused with its reason.
    #[test]
    fn tokens_are_signed_by_the_site_and_checked_by_the_engine() {
        let s = site();
        let t0 = 1_763_450_000;
        let none = |_: &str| None;
        let tok = sign(&claims(&s, "desk", "fp1", Scope::Control, t0), &s.key).unwrap();
        assert!(tok.starts_with("idn1."));
        let c = verify(&tok, &expect(&s, "desk", "fp1", t0 + 10), none).unwrap();
        assert_eq!(
            (c.sub.as_str(), c.scope.clone(), c.exp),
            ("alice", Scope::Control, t0 + 3600)
        );
        assert_eq!(peek(&tok).unwrap(), c);
        // Any machine of the site.
        let any = sign(&claims(&s, "desk", "*", Scope::Read, t0), &s.key).unwrap();
        assert!(verify(&any, &expect(&s, "desk", "fp9", t0), none).is_ok());
        // Wrong engine, machine.
        assert_eq!(
            verify(&tok, &expect(&s, "switcher", "fp1", t0), none),
            Err(TokenError::WrongEngine)
        );
        assert_eq!(
            verify(&tok, &expect(&s, "desk", "fp2", t0), none),
            Err(TokenError::WrongMachine)
        );
        // Another site's key, and a site that isn't this one's.
        let other = site();
        let theirs = sign(
            &claims(&other, "desk", "fp1", Scope::Control, t0),
            &other.key,
        )
        .unwrap();
        assert_eq!(
            verify(&theirs, &expect(&s, "desk", "fp1", t0), none),
            Err(TokenError::Forged)
        );
        let mut c2 = claims(&s, "desk", "fp1", Scope::Control, t0);
        c2.site = other.id.clone();
        let wrong_site = sign(&c2, &s.key).unwrap();
        assert_eq!(
            verify(&wrong_site, &expect(&s, "desk", "fp1", t0), none),
            Err(TokenError::WrongSite)
        );
        // Forged: claims changed after signing (read made control).
        let (_, sig) = tok.rsplit_once('.').unwrap();
        let mut forged_claims = claims(&s, "desk", "fp1", Scope::Control, t0).to_json();
        forged_claims["sub"] = json!("mallory");
        let forged = format!(
            "{PREFIX}{}.{sig}",
            b64url(forged_claims.to_string().as_bytes())
        );
        assert_eq!(
            verify(&forged, &expect(&s, "desk", "fp1", t0), none),
            Err(TokenError::Forged)
        );
        assert_eq!(
            verify("idn1.abc", &expect(&s, "desk", "fp1", t0), none),
            Err(TokenError::Malformed)
        );
        assert_eq!(
            verify("Bearer x", &expect(&s, "desk", "fp1", t0), none),
            Err(TokenError::Malformed)
        );
        // Time: expired, from the future, the skew allowed either way.
        assert_eq!(
            verify(&tok, &expect(&s, "desk", "fp1", t0 + 3600 + SKEW + 1), none),
            Err(TokenError::Expired)
        );
        assert!(verify(&tok, &expect(&s, "desk", "fp1", t0 + 3600 + SKEW), none).is_ok());
        assert_eq!(
            verify(&tok, &expect(&s, "desk", "fp1", t0 - SKEW - 1), none),
            Err(TokenError::NotYet)
        );
        assert!(verify(&tok, &expect(&s, "desk", "fp1", t0 - SKEW), none).is_ok());
        // Not before: issued before the user's, refused; after it, fine.
        assert_eq!(
            verify(&tok, &expect(&s, "desk", "fp1", t0), |u| (u == "alice")
                .then_some(t0 + 1)),
            Err(TokenError::Revoked)
        );
        assert!(verify(&tok, &expect(&s, "desk", "fp1", t0), |_| Some(t0)).is_ok());
        assert!(
            verify(&tok, &expect(&s, "desk", "fp1", t0), |u| (u == "bob")
                .then_some(t0 + 1))
            .is_ok()
        );
    }

    /// Over 12 hours: refused when made, when signed, and when checked (signed anyway).
    #[test]
    fn tokens_live_twelve_hours_at_most() {
        let s = site();
        let t0 = 1_763_450_000;
        assert!(Claims::new(&s.id, "a", "", "desk", "*", Scope::Read, t0, MAX_LIFE).is_ok());
        assert!(Claims::new(&s.id, "a", "", "desk", "*", Scope::Read, t0, MAX_LIFE + 1).is_err());
        assert!(Claims::new(&s.id, "a", "", "mixer", "*", Scope::Read, t0, 60).is_err());
        let mut c = claims(&s, "desk", "*", Scope::Read, t0);
        c.exp = t0 + MAX_LIFE + 1;
        assert!(sign(&c, &s.key).is_err());
        // A token signed past the limit by other means is still refused.
        let body = c.to_json().to_string().into_bytes();
        let sig = s.key.sign(&signed_bytes(&body)).unwrap();
        let long = format!("{PREFIX}{}.{}", b64url(&body), b64url(&sig));
        assert_eq!(
            verify(
                &long,
                &Expect {
                    ca_public: &s.key.public,
                    site: &s.id,
                    engine: "desk",
                    machine: "x",
                    now: t0
                },
                |_| None
            ),
            Err(TokenError::TooLong)
        );
    }

    #[test]
    fn scopes_parse_and_print() {
        for s in [
            "read",
            "control",
            "bus:aux:5",
            "bus:matrix:2",
            "bus:stream",
            "iem:band-1",
        ] {
            assert_eq!(Scope::parse(s).unwrap().as_string(), s);
        }
        for s in ["", "admin", "bus:", "iem:", "bus:a b", "READ"] {
            assert_eq!(Scope::parse(s), None, "{s}");
        }
        assert_eq!(
            not_before_key("Alice Smith").len(),
            NOT_BEFORE_PREFIX.len() + 16
        );
        let mut o = SiteOptions::default();
        o.set(
            &super::super::OptionScope::Site,
            &not_before_key("Alice Smith"),
            "1763450000",
            "",
        )
        .unwrap();
        assert_eq!(not_before(&o, "fp", "Alice Smith"), Some(1_763_450_000));
        assert_eq!(not_before(&o, "fp", "Bob"), None);
    }
}
