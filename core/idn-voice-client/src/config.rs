//! What a client is given: where the server is (QUIC on the link, the WebSocket), how to
//! trust it (each one's certificate SHA-256), who it is (the credential), and a session to
//! resume. From JSON for the C ABI; the enrolment answer's fields keep their names
//! (`urls`, `cert_sha256`, `link`, `link_cert_sha256`), so an app may pass it as it came,
//! with the credential.

use serde_json::Value;

/// How the client reaches the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransportChoice {
    /// QUIC first, the WebSocket when it doesn't connect (the default).
    #[default]
    Auto,
    /// QUIC only.
    Quic,
    /// The WebSocket only.
    WebSocket,
}

/// A session to come back to: the newest `resume` token of a `hello`, and `have` (the
/// last `seq` received from each talker, `TALKER:SEQ,…`). An app that restarts within the
/// site's `resume_s` keeps its channels and gets what it missed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResumeState {
    pub token: String,
    pub have: String,
}

/// A client's configuration (see [`Config::from_json`] for the JSON).
#[derive(Debug, Clone)]
pub struct Config {
    /// The handset's credential (`idnv1.<id>.<secret>`), or a program's API key.
    pub credential: String,
    /// The link's addresses for QUIC (`host:port`, UDP), tried in order.
    pub link: Vec<String>,
    /// The SHA-256 of the link listener's leaf certificate. QUIC is tried only with it.
    pub link_cert_sha256: Option<[u8; 32]>,
    /// The WebSocket's addresses (`wss://…/voice/ws`), tried in order.
    pub urls: Vec<String>,
    /// The SHA-256 of the HTTPS port's leaf certificate; `None`: public authorities.
    pub cert_sha256: Option<[u8; 32]>,
    pub resume: Option<ResumeState>,
    /// The app's name and version (the WebSocket's User-Agent, the QUIC sign-in's).
    pub app: String,
    pub app_version: String,
    pub transport: TransportChoice,
    /// Until the first `hello` tells them: how long QUIC may take to connect, how often
    /// it is tried again while on the WebSocket, and its keep-alive.
    pub link_connect_ms: u64,
    pub link_retry_s: u64,
    pub link_keepalive_ms: u64,
    /// How long a WebSocket may take to connect and say `hello`.
    pub ws_connect_ms: u64,
    /// How much of a talk is kept while the link is down, seconds (0–300): sent when it
    /// comes back. Past it the oldest goes.
    pub hold_mic_s: u64,
    /// The loss the microphone's Opus plans its in-band FEC for, % (0–50; 0: no FEC).
    /// Each packet then carries a coarse copy of the one before, so a frame the server
    /// dropped is rebuilt from the next. Opus adds it only from about 10 % on, out of
    /// the same 24 kbit/s.
    pub fec_loss_pct: u8,
}

impl Config {
    /// A configuration with this credential and the defaults (no addresses yet).
    pub fn new(credential: &str) -> Config {
        Config {
            credential: credential.to_owned(),
            link: Vec::new(),
            link_cert_sha256: None,
            urls: Vec::new(),
            cert_sha256: None,
            resume: None,
            app: String::new(),
            app_version: String::new(),
            transport: TransportChoice::Auto,
            link_connect_ms: 3_000,
            link_retry_s: 60,
            link_keepalive_ms: 5_000,
            ws_connect_ms: 10_000,
            hold_mic_s: 30,
            fec_loss_pct: 10,
        }
    }

    /// From JSON (fields not named here are ignored, so an enrolment answer may be
    /// passed as it is):
    ///
    /// ```json
    /// {"credential": "idnv1.h4kq2x7ma.n3…",
    ///  "link": ["192.168.1.5:29203"], "link_cert_sha256": "3F:1A:…",
    ///  "urls": ["https://192.168.1.5:29203/voice"], "cert_sha256": "3F:1A:…",
    ///  "resume": "q3…", "have": "17:204,22:9",
    ///  "app": "Fleet Driver", "version": "2.4.0",
    ///  "transport": "auto", "link_connect_ms": 3000, "link_retry_s": 60,
    ///  "link_keepalive_ms": 5000, "ws_connect_ms": 10000, "hold_mic_s": 30, "fec_loss_pct": 10}
    /// ```
    ///
    /// `urls` may be the enrolment's base addresses (`https://…/voice`) or WebSocket ones
    /// (`wss://…/voice/ws`); `http://`/`ws://` only where the site allows plain handsets.
    /// Fingerprints: 64 hex digits, colons optional, any case.
    pub fn from_json(text: &str) -> Result<Config, String> {
        let v: Value = serde_json::from_str(text).map_err(|e| format!("config: {e}"))?;
        if !v.is_object() {
            return Err("config: not a JSON object".into());
        }
        let credential = v["credential"].as_str().unwrap_or_default();
        let mut c = Config::new(credential);
        c.link = strings(&v["link"], "link")?;
        c.urls = strings(&v["urls"], "urls")?;
        c.link_cert_sha256 = fingerprint(&v["link_cert_sha256"], "link_cert_sha256")?;
        c.cert_sha256 = fingerprint(&v["cert_sha256"], "cert_sha256")?;
        if let Some(token) = v["resume"].as_str().filter(|t| !t.is_empty()) {
            c.resume = Some(ResumeState {
                token: token.to_owned(),
                have: v["have"].as_str().unwrap_or_default().to_owned(),
            });
        }
        c.app = v["app"].as_str().unwrap_or_default().to_owned();
        c.app_version = v["version"].as_str().unwrap_or_default().to_owned();
        c.transport = match v["transport"].as_str().unwrap_or("auto") {
            "auto" => TransportChoice::Auto,
            "quic" => TransportChoice::Quic,
            "websocket" => TransportChoice::WebSocket,
            t => return Err(format!("transport: auto, quic or websocket, not {t:?}")),
        };
        let num = |key: &str, default: u64| -> Result<u64, String> {
            match &v[key] {
                Value::Null => Ok(default),
                n => n.as_u64().ok_or_else(|| format!("{key}: a whole number")),
            }
        };
        c.link_connect_ms = num("link_connect_ms", c.link_connect_ms)?;
        c.link_retry_s = num("link_retry_s", c.link_retry_s)?;
        c.link_keepalive_ms = num("link_keepalive_ms", c.link_keepalive_ms)?;
        c.ws_connect_ms = num("ws_connect_ms", c.ws_connect_ms)?;
        c.hold_mic_s = num("hold_mic_s", c.hold_mic_s)?;
        c.fec_loss_pct = num("fec_loss_pct", c.fec_loss_pct as u64)?.min(255) as u8;
        c.check()?;
        Ok(c)
    }

    /// Checked as [`Client::connect`](crate::Client::connect) takes it: a credential, an
    /// address for each transport chosen, timings in range. `urls` become WebSocket ones.
    pub fn check(&mut self) -> Result<(), String> {
        let cred = self.credential.trim();
        if cred.is_empty() || cred.len() > 1024 || cred.chars().any(|c| c.is_control()) {
            return Err("credential: missing or not one".into());
        }
        self.credential = cred.to_owned();
        for l in &self.link {
            if split_host_port(l).is_none() {
                return Err(format!("link: {l:?} is not host:port"));
            }
        }
        self.urls = self
            .urls
            .iter()
            .map(|u| websocket_url(u).ok_or_else(|| format!("urls: {u:?} is not an address")))
            .collect::<Result<_, _>>()?;
        let quic = !self.link.is_empty() && self.link_cert_sha256.is_some();
        match self.transport {
            TransportChoice::Quic if !quic => {
                return Err("transport quic: give link and link_cert_sha256".into());
            }
            TransportChoice::WebSocket if self.urls.is_empty() => {
                return Err("transport websocket: give urls".into());
            }
            TransportChoice::Auto if !quic && self.urls.is_empty() => {
                return Err("give urls, or link with link_cert_sha256".into());
            }
            _ => {}
        }
        range("link_connect_ms", self.link_connect_ms, 100, 60_000)?;
        range("link_retry_s", self.link_retry_s, 1, 3_600)?;
        range("link_keepalive_ms", self.link_keepalive_ms, 500, 60_000)?;
        range("ws_connect_ms", self.ws_connect_ms, 1_000, 120_000)?;
        range("hold_mic_s", self.hold_mic_s, 0, 300)?;
        range("fec_loss_pct", self.fec_loss_pct as u64, 0, 50)?;
        Ok(())
    }
}

fn range(key: &str, v: u64, lo: u64, hi: u64) -> Result<(), String> {
    if (lo..=hi).contains(&v) {
        Ok(())
    } else {
        Err(format!("{key}: {lo} to {hi}"))
    }
}

fn strings(v: &Value, key: &str) -> Result<Vec<String>, String> {
    match v {
        Value::Null => Ok(Vec::new()),
        Value::String(s) => Ok(vec![s.clone()]),
        Value::Array(a) => a
            .iter()
            .map(|s| {
                s.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("{key}: a list of strings"))
            })
            .collect(),
        _ => Err(format!("{key}: a list of strings")),
    }
}

fn fingerprint(v: &Value, key: &str) -> Result<Option<[u8; 32]>, String> {
    match v {
        Value::Null => Ok(None),
        Value::String(s) if s.is_empty() => Ok(None),
        Value::String(s) => parse_fingerprint(s)
            .map(Some)
            .ok_or_else(|| format!("{key}: 64 hex digits (colons optional)")),
        _ => Err(format!("{key}: 64 hex digits (colons optional)")),
    }
}

/// A SHA-256 as people and the server write it: 64 hex digits, colons optional, any case.
/// (`idn_common::hex::unhex`'s work; this crate depends on nothing of IDN's own.)
pub fn parse_fingerprint(s: &str) -> Option<[u8; 32]> {
    let digits: Vec<u8> = s.bytes().filter(|&b| b != b':').collect();
    if digits.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in digits.chunks(2).enumerate() {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Some(out)
}

/// `host:port` (IPv6 in brackets) split; the port given.
pub(crate) fn split_host_port(s: &str) -> Option<(String, u16)> {
    let (host, port) = s.trim().rsplit_once(':')?;
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if host.is_empty() || host.contains(['/', ' ', '[', ']']) {
        return None;
    }
    Some((host.to_owned(), port.parse().ok().filter(|&p| p != 0)?))
}

/// A server address as a WebSocket one: `https://H/voice` → `wss://H/voice/ws`,
/// `http://` → `ws://`; `wss://`/`ws://` as they are. A trailing `/` is dropped.
pub(crate) fn websocket_url(u: &str) -> Option<String> {
    let u = u.trim().trim_end_matches('/');
    let (given, rest) = u.split_once("://")?;
    let given = given.to_ascii_lowercase();
    let scheme = match given.as_str() {
        "https" | "wss" => "wss",
        "http" | "ws" => "ws",
        _ => return None,
    };
    if rest.is_empty() || rest.contains(['?', '#', ' ']) {
        return None;
    }
    // A WebSocket address is taken as it is; a base address gets the socket's path.
    let path = if given.starts_with("ws") || rest.ends_with("/ws") {
        rest.to_owned()
    } else {
        format!("{rest}/ws")
    };
    let url = format!("{scheme}://{path}");
    parse_url(&url).map(|_| url)
}

/// A WebSocket address's parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Url {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    /// From the `/` on.
    pub path: String,
}

pub(crate) fn parse_url(u: &str) -> Option<Url> {
    let (scheme, rest) = u.split_once("://")?;
    let tls = match scheme {
        "wss" => true,
        "ws" => false,
        _ => return None,
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_owned()),
        None => (rest, "/".to_owned()),
    };
    let default = if tls { 443 } else { 80 };
    // `[v6]:port`, `[v6]`, `host:port`, `host`.
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (h, after) = v6.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None if after.is_empty() => default,
            None => return None,
        };
        (h.to_owned(), port)
    } else if let Some((h, p)) = authority.rsplit_once(':') {
        (h.to_owned(), p.parse().ok()?)
    } else {
        (authority.to_owned(), default)
    };
    if host.is_empty() || host.contains('@') {
        return None;
    }
    Some(Url {
        tls,
        host,
        port,
        path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FP: &str = "3F:1A:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:aa:bb:cc:dd";

    #[test]
    fn an_enrolment_answer_with_the_credential_is_a_configuration() {
        let c = Config::from_json(&format!(
            r#"{{"handset": {{"id": "h4kq2x7ma", "name": "Truck 12"}},
               "credential": " idnv1.h4kq2x7ma.secret ",
               "urls": ["https://192.168.1.5:29203/voice", "https://voice.example.com/voice/"],
               "cert_sha256": "{FP}",
               "link": ["192.168.1.5:29203", "[fe80::1]:29203"],
               "link_cert_sha256": "{}",
               "resume": "tok", "have": "17:204",
               "app": "Fleet Driver", "version": "2.4.0"}}"#,
            FP.replace(':', "")
        ))
        .unwrap();
        assert_eq!(c.credential, "idnv1.h4kq2x7ma.secret");
        assert_eq!(
            c.urls,
            [
                "wss://192.168.1.5:29203/voice/ws",
                "wss://voice.example.com/voice/ws"
            ]
        );
        assert_eq!(c.cert_sha256.unwrap()[0], 0x3f);
        assert_eq!(c.cert_sha256, c.link_cert_sha256);
        assert_eq!(c.link.len(), 2);
        assert_eq!(
            c.resume,
            Some(ResumeState {
                token: "tok".into(),
                have: "17:204".into()
            })
        );
        assert_eq!(c.transport, TransportChoice::Auto);
        assert_eq!(
            (c.link_connect_ms, c.link_retry_s, c.link_keepalive_ms),
            (3000, 60, 5000)
        );
        assert_eq!(c.hold_mic_s, 30);
        assert_eq!(c.fec_loss_pct, 10);
    }

    #[test]
    fn a_wrong_configuration_says_what_is_wrong() {
        let bad = |j: &str| Config::from_json(j).unwrap_err();
        assert!(bad("[]").contains("object"));
        assert!(bad(r#"{"urls": ["https://h/voice"]}"#).contains("credential"));
        assert!(bad(r#"{"credential": "c"}"#).contains("urls"));
        assert!(bad(r#"{"credential": "c", "urls": ["ftp://h"]}"#).contains("ftp"));
        assert!(
            bad(r#"{"credential": "c", "urls": ["https://h"], "cert_sha256": "12"}"#)
                .contains("cert_sha256")
        );
        assert!(
            bad(r#"{"credential": "c", "link": ["h:1"], "transport": "quic"}"#)
                .contains("link_cert_sha256")
        );
        assert!(bad(r#"{"credential": "c", "link": ["nope"]}"#).contains("host:port"));
        assert!(
            bad(r#"{"credential": "c", "urls": ["https://h"], "link_retry_s": 0}"#)
                .contains("link_retry_s")
        );
        assert!(
            bad(r#"{"credential": "c", "urls": ["https://h"], "hold_mic_s": 301}"#)
                .contains("hold_mic_s")
        );
        assert!(
            bad(r#"{"credential": "c", "urls": ["https://h"], "fec_loss_pct": 51}"#)
                .contains("fec_loss_pct")
        );
        assert!(
            bad(r#"{"credential": "c", "urls": ["https://h"], "transport": "udp"}"#)
                .contains("transport")
        );
        // QUIC without its pin waits for hello's; the WebSocket alone is enough.
        let c = Config::from_json(
            r#"{"credential": "c", "link": ["h:1"], "urls": ["ws://h:8/voice/ws"]}"#,
        )
        .unwrap();
        assert_eq!(c.link_cert_sha256, None);
        assert_eq!(c.urls, ["ws://h:8/voice/ws"]);
    }

    #[test]
    fn addresses_are_read_as_written() {
        assert_eq!(
            parse_url("wss://[::1]:29203/voice/ws"),
            Some(Url {
                tls: true,
                host: "::1".into(),
                port: 29203,
                path: "/voice/ws".into()
            })
        );
        assert_eq!(parse_url("ws://h/voice/ws").unwrap().port, 80);
        assert_eq!(parse_url("wss://h").unwrap().path, "/");
        assert_eq!(parse_url("wss://u@h/"), None);
        assert_eq!(
            split_host_port("[fe80::1]:29203"),
            Some(("fe80::1".into(), 29203))
        );
        assert_eq!(split_host_port("h:0"), None);
        assert_eq!(
            websocket_url("HTTP://h:8080/voice").unwrap(),
            "ws://h:8080/voice/ws"
        );
        assert_eq!(parse_fingerprint(&"ab".repeat(32)), Some([0xab; 32]));
        assert_eq!(parse_fingerprint(&"zz".repeat(32)), None);
    }
}
