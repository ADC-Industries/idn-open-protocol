//! A request's headers read one way everywhere: the bearer token, a cookie, the `Set-Cookie`
//! an answer gives, a Host's name and port, and whether a name is this machine's loopback.
//! For this module's own [`Request`](super::Request) and for the `http` crate's
//! `HeaderMap` (axum's), so every IDN server reads them alike (each had its own copy,
//! differing on case, spaces and repeated headers).

use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
use std::time::Duration;

/// Headers a request carries, as the helpers here read them.
pub trait Headers {
    /// Every value of header `name` (given in lower case), in the order they came.
    fn header_values<'a>(&'a self, name: &str) -> impl Iterator<Item = &'a str>;
}

/// Header names in lower case, one value each (repeated cookies joined with `; `, as
/// [`serve`](super::serve) reads them).
impl Headers for HashMap<String, String> {
    fn header_values<'a>(&'a self, name: &str) -> impl Iterator<Item = &'a str> {
        self.get(name).map(String::as_str).into_iter()
    }
}

impl Headers for super::Request {
    fn header_values<'a>(&'a self, name: &str) -> impl Iterator<Item = &'a str> {
        self.headers.header_values(name)
    }
}

/// The `http` crate's (axum's, hyper's); values that aren't text are left out.
impl Headers for tungstenite::http::HeaderMap {
    fn header_values<'a>(&'a self, name: &str) -> impl Iterator<Item = &'a str> {
        self.get_all(name)
            .into_iter()
            .filter_map(|v| v.to_str().ok())
    }
}

/// The token of `Authorization: Bearer …`: the scheme in any case, the token trimmed; an
/// empty one, or another scheme, is none.
pub fn bearer<H: Headers + ?Sized>(h: &H) -> Option<&str> {
    let v = h.header_values("authorization").next()?.trim();
    let (scheme, token) = v.split_once(char::is_whitespace)?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

/// Cookie `name`'s value: from every `Cookie` header (HTTP/2 and some proxies send one a
/// cookie), its `;`-separated pairs trimmed; the first of that name.
pub fn cookie<'a, H: Headers + ?Sized>(h: &'a H, name: &str) -> Option<&'a str> {
    h.header_values("cookie")
        .flat_map(|v| v.split(';'))
        .find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k.trim() == name).then(|| v.trim())
        })
}

/// A cookie's `SameSite`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SameSite {
    /// Sent only with requests from the site's own pages (a panel's session).
    Strict,
    /// Also on following a link from elsewhere (a sign-in link).
    Lax,
    /// Also inside another site's page; always with `Secure` (browsers drop it without).
    None,
}

/// A `Set-Cookie` value, made with [`set_cookie`]; written out by `Display`.
#[derive(Debug, Clone)]
pub struct SetCookie<'a> {
    name: &'a str,
    value: &'a str,
    path: &'a str,
    max_age: Option<Duration>,
    same_site: SameSite,
    http_only: bool,
    secure: bool,
}

/// A `Set-Cookie` value for `name` = `value` (both cookie-safe: a token, an id), as the
/// panels' sessions set them: `Path=/`, `HttpOnly`, `SameSite=Strict`, until the browser
/// closes, not `Secure`; each changed by its method. `set_cookie(name, "").max_age(ZERO)`
/// removes it.
pub fn set_cookie<'a>(name: &'a str, value: &'a str) -> SetCookie<'a> {
    SetCookie {
        name,
        value,
        path: "/",
        max_age: None,
        same_site: SameSite::Strict,
        http_only: true,
        secure: false,
    }
}

impl<'a> SetCookie<'a> {
    /// Kept this long (whole seconds; zero removes it).
    pub fn max_age(mut self, d: Duration) -> Self {
        self.max_age = Some(d);
        self
    }

    pub fn same_site(mut self, s: SameSite) -> Self {
        self.same_site = s;
        self
    }

    /// Sent only over HTTPS (set it whenever the page came over HTTPS).
    pub fn secure(mut self, on: bool) -> Self {
        self.secure = on;
        self
    }

    /// Out of reach of the page's scripts (on by default).
    pub fn http_only(mut self, on: bool) -> Self {
        self.http_only = on;
        self
    }

    pub fn path(mut self, p: &'a str) -> Self {
        self.path = p;
        self
    }
}

impl fmt::Display for SetCookie<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}={}; Path={}", self.name, self.value, self.path)?;
        if self.http_only {
            f.write_str("; HttpOnly")?;
        }
        f.write_str(match self.same_site {
            SameSite::Strict => "; SameSite=Strict",
            SameSite::Lax => "; SameSite=Lax",
            SameSite::None => "; SameSite=None",
        })?;
        if let Some(d) = self.max_age {
            write!(f, "; Max-Age={}", d.as_secs())?;
        }
        if self.secure || self.same_site == SameSite::None {
            f.write_str("; Secure")?;
        }
        Ok(())
    }
}

/// The client's own address: `peer` (who connected), unless `peer` is a proxy `trusted`
/// says is this server's (a tunnel on this machine). Then `X-Forwarded-For` read from the
/// right: each proxy adds the address it was reached from, so the first not trusted is
/// the client; the leftmost is what the client itself sent (anyone could pick their
/// address for a login limit). With no such entry, `CF-Connecting-IP` or `X-Real-IP`.
/// A value that isn't an address is the unspecified one (all such counted together).
pub fn client_ip<H: Headers + ?Sized>(
    h: &H,
    peer: IpAddr,
    trusted: impl Fn(IpAddr) -> bool,
) -> IpAddr {
    if !trusted(peer) {
        return peer;
    }
    let parse = |a: &str| {
        a.trim()
            .parse::<std::net::SocketAddr>()
            .map(|s| s.ip())
            .or_else(|_| a.trim().parse::<IpAddr>())
            .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
    };
    let hops: Vec<&str> = h
        .header_values("x-forwarded-for")
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .collect();
    if let Some(a) = hops
        .iter()
        .rev()
        .find(|a| a.parse::<IpAddr>().map_or(true, |ip| !trusted(ip)))
    {
        return parse(a);
    }
    for k in ["cf-connecting-ip", "x-real-ip"] {
        if let Some(v) = h.header_values(k).next() {
            return parse(v);
        }
    }
    peer
}

/// A Host (or `HOST[:PORT]` typed in) as its name and port: `[::1]:8080`, `[::1]`, `::1`
/// (a bare IPv6 address: no port), `host:80`, `host`; the brackets taken off. A port that
/// isn't a number splits nothing: the whole text is the name, with no port.
pub fn split_host(h: &str) -> (&str, Option<u16>) {
    let h = h.trim();
    if let Some(rest) = h.strip_prefix('[') {
        return match rest.split_once(']') {
            Some((name, "")) => (name, None),
            // (`[::1]:`: an empty port is the default one.)
            Some((name, ":")) => (name, None),
            Some((name, after)) => match after.strip_prefix(':').map(str::parse::<u16>) {
                Some(Ok(p)) => (name, Some(p)),
                _ => (h, None),
            },
            None => (h, None),
        };
    }
    match h.split_once(':') {
        // One colon: a port (several: a bare IPv6 address).
        Some((name, p)) if !p.contains(':') => match p.parse() {
            Ok(p) => (name, Some(p)),
            // (`host:`: an empty port is the default one, as URL parsers read it.)
            Err(_) if p.is_empty() => (name, None),
            Err(_) => (h, None),
        },
        _ => (h, None),
    }
}

/// Whether `name` (a Host's, without its port) is this machine's loopback: `localhost`
/// and the names under it (RFC 6761: browsers keep them on the machine), `127.0.0.0/8`,
/// `::1` (bracketed or not) and `::ffff:127.x.y.z`; any case, a trailing dot allowed.
pub fn is_loopback_name(name: &str) -> bool {
    let n = name.trim().trim_end_matches('.');
    let n = n
        .strip_prefix('[')
        .and_then(|r| r.strip_suffix(']'))
        .unwrap_or(n);
    let lower = n.to_ascii_lowercase();
    if lower == "localhost" || lower.len() > ".localhost".len() && lower.ends_with(".localhost") {
        return true;
    }
    match n.parse::<IpAddr>() {
        Ok(IpAddr::V4(a)) => a.is_loopback(),
        Ok(IpAddr::V6(a)) => a.is_loopback() || a.to_ipv4_mapped().is_some_and(|v| v.is_loopback()),
        Err(_) => false,
    }
}

/// [`is_loopback_name`] for a Host header checked against DNS rebinding: `localhost` and
/// loopback addresses only, not the names under `localhost` (a browser or resolver that
/// sends `x.localhost` to DNS lets an attacker's name pass, rebound to 127.0.0.1).
pub fn is_loopback_host(name: &str) -> bool {
    let n = name.trim().trim_end_matches('.');
    let bare = n
        .strip_prefix('[')
        .and_then(|r| r.strip_suffix(']'))
        .unwrap_or(n);
    (bare.eq_ignore_ascii_case("localhost") || bare.parse::<IpAddr>().is_ok())
        && is_loopback_name(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn hosts_split_with_and_without_ports() {
        assert_eq!(split_host("[::1]:8080"), ("::1", Some(8080)));
        assert_eq!(split_host("[::1]"), ("::1", None));
        assert_eq!(split_host("::1"), ("::1", None));
        assert_eq!(split_host("fe80::1:2"), ("fe80::1:2", None));
        assert_eq!(split_host("host:80"), ("host", Some(80)));
        assert_eq!(split_host(" host "), ("host", None));
        assert_eq!(split_host("10.0.0.5:29215"), ("10.0.0.5", Some(29215)));
        // Not a port: nothing split.
        assert_eq!(split_host("host:abc"), ("host:abc", None));
        // An empty port: the default one (as URL parsers read it).
        assert_eq!(split_host("host:"), ("host", None));
        assert_eq!(split_host("host:99999"), ("host:99999", None));
        assert_eq!(split_host("[::1]:x"), ("[::1]:x", None));
        assert_eq!(split_host("[::1"), ("[::1", None));
        assert_eq!(split_host(""), ("", None));
    }

    #[test]
    fn loopback_names() {
        for n in [
            "localhost",
            "LocalHost",
            "localhost.",
            "panel.localhost",
            "127.0.0.1",
            "127.1.2.3",
            "::1",
            "[::1]",
            "::ffff:127.0.0.1",
        ] {
            assert!(is_loopback_name(n), "{n}");
        }
        for n in [
            "",
            "localhost.example",
            "evil-localhost",
            ".localhost",
            "10.0.0.1",
            "[::2]",
            "fe80::1",
            "0.0.0.0",
            "::",
            "é.localhos",
        ] {
            assert!(!is_loopback_name(n), "{n}");
        }
    }

    #[test]
    fn bearer_tokens() {
        let b = |v: &str| bearer(&map(&[("authorization", v)])).map(str::to_owned);
        assert_eq!(b("Bearer abc"), Some("abc".into()));
        assert_eq!(b("bearer abc"), Some("abc".into()));
        assert_eq!(b("BEARER   abc  "), Some("abc".into()));
        assert_eq!(b("  Bearer\tabc"), Some("abc".into()));
        assert_eq!(b("Bearer "), None);
        assert_eq!(b("Bearer"), None);
        assert_eq!(b("Bearerabc"), None);
        assert_eq!(b("Basic YWxhZGRpbjpvcGVu"), None);
        assert_eq!(b(""), None);
        assert_eq!(bearer(&map(&[])), None);
        // axum's map, the same rules.
        let mut h = tungstenite::http::HeaderMap::new();
        h.insert("authorization", "bearer idn1.x".parse().unwrap());
        assert_eq!(bearer(&h), Some("idn1.x"));
    }

    #[test]
    fn cookies_from_every_header() {
        let h = map(&[("cookie", "a=1; idn_site = s1 ;b=2")]);
        assert_eq!(cookie(&h, "idn_site"), Some("s1"));
        assert_eq!(cookie(&h, "b"), Some("2"));
        assert_eq!(cookie(&h, "c"), None);
        assert_eq!(cookie(&h, "idn"), None);
        let mut m = tungstenite::http::HeaderMap::new();
        m.append("cookie", "a=1".parse().unwrap());
        m.append("cookie", "idn_sm_session=t2; x=".parse().unwrap());
        assert_eq!(cookie(&m, "idn_sm_session"), Some("t2"));
        assert_eq!(cookie(&m, "x"), Some(""));
        assert_eq!(cookie(&m, "a"), Some("1"));
    }

    #[test]
    fn set_cookie_values() {
        assert_eq!(
            set_cookie("idn_site", "abc")
                .max_age(Duration::from_secs(3600))
                .secure(true)
                .to_string(),
            "idn_site=abc; Path=/; HttpOnly; SameSite=Strict; Max-Age=3600; Secure"
        );
        assert_eq!(
            set_cookie("s", "").max_age(Duration::ZERO).to_string(),
            "s=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0"
        );
        assert_eq!(
            set_cookie("v", "t").same_site(SameSite::Lax).to_string(),
            "v=t; Path=/; HttpOnly; SameSite=Lax"
        );
        // SameSite=None only with Secure.
        assert_eq!(
            set_cookie("v", "t")
                .same_site(SameSite::None)
                .http_only(false)
                .path("/voice")
                .to_string(),
            "v=t; Path=/voice; SameSite=None; Secure"
        );
    }

    #[test]
    fn an_empty_port_is_the_default_and_host_checks_are_strict() {
        assert_eq!(split_host("127.0.0.1:"), ("127.0.0.1", None));
        assert_eq!(split_host("[::1]:"), ("::1", None));
        assert_eq!(split_host("host:abc"), ("host:abc", None));
        assert!(is_loopback_host("localhost") && is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("[::1]") && is_loopback_host("LOCALHOST."));
        assert!(!is_loopback_host("evil.localhost") && is_loopback_name("evil.localhost"));
        assert!(!is_loopback_host("example.com"));
    }

    #[test]
    fn the_client_behind_a_trusted_proxy() {
        let peer: IpAddr = "127.0.0.1".parse().unwrap();
        let mut h = HashMap::new();
        h.insert("x-forwarded-for".to_owned(), "9.9.9.9, 1.2.3.4".to_owned());
        let lo = |ip: IpAddr| ip.is_loopback();
        // The rightmost entry not a trusted proxy: 1.2.3.4 (9.9.9.9 is what the client said).
        assert_eq!(
            client_ip(&h, peer, lo),
            "1.2.3.4".parse::<IpAddr>().unwrap()
        );
        // Not through a trusted proxy: the peer itself, whatever the headers say.
        let other: IpAddr = "5.6.7.8".parse().unwrap();
        assert_eq!(client_ip(&h, other, lo), other);
        // Nothing forwarded: the peer.
        assert_eq!(client_ip(&HashMap::new(), peer, lo), peer);
    }
}
