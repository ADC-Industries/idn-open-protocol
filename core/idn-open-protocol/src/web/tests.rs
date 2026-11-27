//! Tests of the web server's parts.

use super::server::read_request;
use super::*;

#[test]
fn hand_off_tokens_are_good_once_and_only_with_the_key() {
    let a = HandOff::random();
    let b = HandOff::from_hex(&a.key_hex()).unwrap();
    let t = a.issue();
    assert!(b.take(&t), "the other program, same key");
    assert!(!b.take(&t), "once only");
    assert!(!HandOff::random().take(&a.issue()), "another key");
    assert!(!a.take("1.2.3") && !a.take("") && !a.take(&format!("{}x", a.issue())));
}

#[test]
fn loopback_panels_answer_only_to_their_own_address() {
    assert!(loopback_host(Some("127.0.0.1:29212"), 29212));
    assert!(loopback_host(Some("LocalHost:29212"), 29212));
    assert!(lan_host_ok(Some("192.168.1.20:8090")));
    assert!(lan_host_ok(Some("[fe80::1]:8090")));
    assert!(lan_host_ok(Some("switcher:8090")));
    assert!(lan_host_ok(Some("switcher.local:8090")));
    assert!(!lan_host_ok(Some("evil.example.com:8090")));
    // This machine's name under a domain that isn't the system's: refused.
    let me = crate::discovery::hostname().to_ascii_lowercase();
    assert!(!lan_host_ok(Some(&format!(
        "{me}.attacker-example.invalid:8090"
    ))));
    assert!(!lan_host_ok(None));
    assert!(loopback_host(Some("[::1]:29212"), 29212));
    assert!(!loopback_host(Some("evil.example:29212"), 29212));
    assert!(!loopback_host(Some("127.0.0.1:7976"), 29212));
    assert!(!loopback_host(Some("127.0.0.1"), 29212), "no port");
    // Names under localhost: not for a Host check (DNS rebinding through a resolver
    // that sends them to DNS).
    assert!(!loopback_host(Some("panel.localhost:29212"), 29212));
    assert!(loopback_host(Some("localhost:29212"), 29212));
    assert!(!loopback_host(None, 29212));
    assert!(
        !lan_host_ok(Some("[evil.example]:8090")),
        "brackets: IPv6 only"
    );
}

#[test]
fn passwords_hash_and_verify() {
    let h = hash_password("St Mark's AV").unwrap();
    assert!(h.starts_with("$argon2id$"));
    assert!(verify_password(&h, "St Mark's AV"));
    assert!(!verify_password(&h, "st mark's av"));
    assert!(!verify_password("not a hash", "x"));
}

#[test]
fn changes_need_the_header_and_the_own_origin() {
    let mut r = Request {
        method: "POST".into(),
        path: "/".into(),
        query: String::new(),
        headers: HashMap::new(),
        body: Vec::new(),
        peer: "127.0.0.1".parse().unwrap(),
        tls: true,
    };
    r.headers.insert("host".into(), "site:29202".into());
    assert!(!r.may_change("X-IDN-Node"));
    r.headers.insert("x-idn-node".into(), "1".into());
    assert!(r.may_change("X-IDN-Node"));
    r.headers
        .insert("origin".into(), "https://evil.example".into());
    assert!(!r.may_change("X-IDN-Node"));
    r.headers
        .insert("origin".into(), "https://site:29202".into());
    assert!(r.may_change("X-IDN-Node"));
}

#[test]
fn the_password_gate() {
    let g =
        PasswordGate::new("open sesame", Duration::from_secs(60), "t_cookie", "X-Test").unwrap();
    let req = |body: &str, cookie: Option<&str>| {
        let mut r = Request {
            method: "POST".into(),
            path: "/api/v1/login".into(),
            query: String::new(),
            headers: HashMap::new(),
            body: body.as_bytes().to_vec(),
            peer: "10.0.0.9".parse().unwrap(),
            tls: true,
        };
        r.headers.insert("x-test".into(), "1".into());
        if let Some(c) = cookie {
            r.headers.insert("cookie".into(), c.into());
        }
        r
    };
    let r = g
        .handle(&req(r#"{"password":"nope"}"#, None), &|_| {})
        .unwrap();
    assert_eq!(r.status, "403 Forbidden");
    let r = g
        .handle(&req(r#"{"password":"open sesame"}"#, None), &|_| {})
        .unwrap();
    assert_eq!(r.status, "200 OK");
    let set = r
        .headers
        .iter()
        .find(|(k, _)| k == "Set-Cookie")
        .map(|(_, v)| v.clone())
        .unwrap();
    assert!(set.contains("Secure") && set.contains("HttpOnly"));
    let cookie = set.split(';').next().unwrap().to_owned();
    let mut get = req("", Some(&cookie));
    get.method = "GET".into();
    get.path = "/api/v1/state".into();
    assert!(g.allowed(&get));
    assert!(g.handle(&get, &|_| {}).is_none(), "not a login request");
    assert!(!g.allowed(&req("", Some("t_cookie=forged"))));
    // Five wrong passwords block the address, also when they come at once (each is
    // counted before its slow check).
    let g = std::sync::Arc::new(g);
    let threads: Vec<_> = (0..7)
        .map(|_| {
            let (g, r) = (g.clone(), req(r#"{"password":"x"}"#, None));
            std::thread::spawn(move || g.handle(&r, &|_| {}).unwrap().status)
        })
        .collect();
    let refused = threads
        .into_iter()
        .map(|t| t.join().unwrap())
        .filter(|s| *s == "429 Too Many Requests")
        .count();
    // (The wrong one before the right one stays counted: a right password takes back
    // only its own try.)
    assert_eq!(refused, 3, "7 at once: exactly 4 more tried");
    let r = g
        .handle(&req(r#"{"password":"open sesame"}"#, None), &|_| {})
        .unwrap();
    assert_eq!(r.status, "429 Too Many Requests");
}

#[test]
fn repeated_cookie_headers_are_all_read() {
    let raw = b"GET / HTTP/1.1\r\nHost: a\r\nCookie: x=1\r\ncookie: idn_site=s2\r\nHost: b\r\n\r\n";
    let r = read_request(
        &mut &raw[..],
        "127.0.0.1".parse().unwrap(),
        false,
        &[],
        None,
    )
    .unwrap();
    assert_eq!(r.cookie("x"), Some("1"));
    assert_eq!(r.cookie("idn_site"), Some("s2"));
    assert_eq!(r.header("host"), Some("b"), "any other header: the last");
}

#[test]
fn json_strings_escape() {
    assert_eq!(
        json_str("a\"b\\c\n</script>"),
        "\"a\\\"b\\\\c\\n\\u003c/script>\""
    );
}
