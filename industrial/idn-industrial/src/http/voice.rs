//! IDN Voice in the Site Manager (spec/extensions/idn-voice.md §V.1): mounted at `/voice`
//! on the HTTP port (through the Cloudflare Tunnel too), with this Site Manager's users,
//! roles, audit and MQTT (`idn/site/voice/…`, §V.10); and over HTTPS on a port of its own
//! (`IDN_VOICE_HTTPS`, 29203) with a certificate of its own, for handsets on the plant's
//! network (§I.18, §V.13.2); and over IDN's link (QUIC) on UDP at the same address, for
//! handsets that change networks (§V.14).

use super::{App, Role};
use idn_common::switches;
use idn_voice::{Host, User, Voice, VoiceConfig};
use std::sync::Arc;

pub(super) struct IndHost {
    app: App,
}

fn user(name: &str, role: Role) -> User {
    User {
        name: name.to_owned(),
        roles: vec![role.as_str().to_owned()],
        admin: role == Role::Admin,
    }
}

impl Host for IndHost {
    fn session_user(&self, h: &axum::http::HeaderMap) -> Option<User> {
        // The API key: a program (e.g. a dispatch system), with every right.
        if super::api_key_given(&self.app, h) {
            return Some(user("api", Role::Admin));
        }
        // The web UI's session (its cookie is SameSite=Strict).
        let token = super::session_of(h)?;
        self.app.ui.live(token).map(|s| user(&s.user, s.role))
    }

    fn login(&self, name: &str, password: &str, from: std::net::IpAddr) -> Option<User> {
        let name = name.trim();
        // The web UI's limits too (per address and per name), so a guess at a name through
        // Voice counts as one through the UI does.
        if !self.app.ui.logins.attempt_as(from, name) {
            return None;
        }
        let r = self.app.sm.verify_user(name, password)?;
        self.app.ui.logins.succeeded_as(from, name);
        Some(user(if name.is_empty() { "admin" } else { name }, r))
    }

    fn users(&self) -> Vec<User> {
        let mut list: Vec<User> = self
            .app
            .sm
            .users()
            .unwrap_or_default()
            .into_iter()
            .map(|(n, r)| user(&n, r))
            .collect();
        if !list.iter().any(|u| u.name == "admin") {
            list.push(user("admin", Role::Admin));
        }
        list
    }

    fn audit(&self, who: &str, action: &str, detail: serde_json::Value) {
        self.app.sm.audit(who, "", action, detail);
    }

    fn trusted_proxy(&self, peer: std::net::IpAddr) -> bool {
        // (The tunnel on this machine, or one named in `IDN_TRUST_PROXY`.)
        super::trusted_proxy(peer)
    }

    fn is_program(&self, h: &axum::http::HeaderMap) -> bool {
        // The API key's user (`api`) is no site account: its sessions aren't signed out
        // for missing from the users (they were, within seconds).
        super::api_key_given(&self.app, h)
    }

    fn publish(&self, topic: &str, body: &serde_json::Value, retain: bool) {
        let msg = crate::mqtt::Msg::new(
            &format!("idn/site/voice/{topic}"),
            body.to_string().into_bytes(),
            1,
            retain,
        );
        let _ = self.app.sm.broker().publish(&msg);
    }
}

/// Starts Voice (`IDN_VOICE_UDP`: its WebRTC port, default 29207, `off`: WebSocket only)
/// and gives its routes; `None` if it can't start (logged).
pub(super) fn start(app: &App) -> Option<axum::Router> {
    let mut cfg = VoiceConfig::new(app.sm.data_dir().join("voice"));
    cfg.udp = idn_voice::udp_setting(switches::value("IDN_VOICE_UDP").as_deref(), cfg.udp);
    // IEM feeds: idn-codec beside this program (a Docker image has none), to this server.
    let codec = std::env::current_exe().ok().and_then(|e| {
        let p = e.with_file_name(if cfg!(windows) {
            "idn-codec.exe"
        } else {
            "idn-codec"
        });
        p.exists().then_some(p)
    });
    let http = switches::value("IDN_HTTP").unwrap_or_else(|| "0.0.0.0:8080".into());
    cfg.codec = codec;
    cfg.feed_url = Some(format!("ws://{}/voice/ws", feed_host(&http)));
    // Handsets (§V.13) are told the site's public addresses too: those machines are given
    // (`IDN_PUBLIC_URLS`), as Voice's (`wss://hub.example.com/idn/link` →
    // `https://hub.example.com/voice`). No certificate of Voice's own here (plain HTTP,
    // or a tunnel's from a public authority): `IDN_VOICE_CERT_SHA256` names a proxy's.
    for u in voice_urls(&switches::value("IDN_PUBLIC_URLS").unwrap_or_default()) {
        if !cfg.urls.contains(&u) {
            cfg.urls.push(u);
        }
    }
    // HTTPS of its own for handsets on the plant's network (a credential never crosses it
    // in the clear): a self-signed certificate made once in Voice's folder (so the site's
    // backup carries it), its SHA-256 what handsets pin.
    let https = https_setting(switches::value("IDN_VOICE_HTTPS").as_deref());
    let tls = match https {
        Some(_) => match std::fs::create_dir_all(&cfg.dir).and_then(|()| {
            idn_open_protocol::web::self_signed(&cfg.dir, "IDN Site Manager | Industrial (Voice)")
        }) {
            Ok((tls, fp)) => {
                if let Some(fp) = idn_voice::model::fingerprint_text(&fp) {
                    cfg.cert_sha256 = Some(fp);
                }
                Some(tls)
            }
            Err(e) => {
                idn_open_protocol::log::log(&format!(
                    "site manager: Voice HTTPS off (its certificate: {e})"
                ));
                None
            }
        },
        None => None,
    };
    match Voice::start(cfg, Arc::new(IndHost { app: app.clone() })) {
        Ok(v) => {
            app.sm.set_voice(&v.hub);
            if let (Some(addr), Some(tls)) = (https, tls) {
                serve_https(&v, addr, tls);
            }
            // The link at the HTTPS port's address (the link takes its TCP port's number;
            // `IDN_VOICE_HTTPS=off` turns both off).
            if let Some(l) = https.and_then(|at| serve_link(&v, at)) {
                app.sm.hold_voice_link(l);
            }
            Some(v.router())
        }
        Err(e) => {
            idn_open_protocol::log::log(&format!("site manager: Voice off: {e}"));
            None
        }
    }
}

/// `IDN_VOICE_HTTPS` as set: `ADDR:PORT`, a bare `PORT` (every address) or off; unset,
/// every address on 29203 (the Site Managers' Voice port, as Media's).
fn https_setting(v: Option<&str>) -> Option<std::net::SocketAddr> {
    let default = std::net::SocketAddr::from(([0, 0, 0, 0], idn_common::ports::SITE_VOICE));
    let Some(v) = v.map(str::trim).filter(|v| !v.is_empty()) else {
        return Some(default);
    };
    if idn_common::switches::says_off(v) {
        return None;
    }
    if let Ok(a) = v.parse() {
        return Some(a);
    }
    if let Ok(p) = v.parse::<u16>() {
        return Some(std::net::SocketAddr::from(([0, 0, 0, 0], p)));
    }
    idn_open_protocol::log::log(&format!(
        "IDN_VOICE_HTTPS={v:?}: not ADDR:PORT, PORT or off; {default} kept"
    ));
    Some(default)
}

/// Voice's HTTPS listener on this runtime (the HTTP port's); logged if it can't.
fn serve_https(v: &Voice, addr: std::net::SocketAddr, tls: Arc<rustls::ServerConfig>) {
    let r = (|| -> std::io::Result<()> {
        let rt = tokio::runtime::Handle::try_current().map_err(idn_common::err::other)?;
        let l = std::net::TcpListener::bind(addr)?;
        v.serve_tls(l, tls, &rt)
    })();
    match r {
        Ok(()) => idn_open_protocol::log::log(&format!(
            "site manager: Voice over HTTPS on {addr} (certificate SHA-256 {})",
            v.cfg.cert_sha256.as_deref().unwrap_or("?")
        )),
        Err(e) => idn_open_protocol::log::log(&format!(
            "site manager: Voice HTTPS off ({addr}: {e}); handsets need the tunnel"
        )),
    }
}

/// Voice over IDN's link (§V.14) on UDP `at`, on this runtime, with a link key of Voice's
/// own (`voice/link.key`: made once, the site's backup carries it, so handsets' pins
/// hold); `None` if it can't (logged: handsets then use the WebSocket).
fn serve_link(v: &Voice, at: std::net::SocketAddr) -> Option<idn_voice::VoiceLink> {
    let r = tokio::runtime::Handle::try_current()
        .map_err(idn_common::err::other)
        .and_then(|rt| {
            let link = idn_open_protocol::link::LinkConfig::in_dir(&v.cfg.dir)?;
            v.serve_link(at, &link, &rt)
        });
    match r {
        Ok(l) => {
            idn_open_protocol::log::log(&format!(
                "site manager: Voice over IDN's link on UDP {} (certificate SHA-256 {})",
                l.port(),
                l.cert_sha256()
            ));
            Some(l)
        }
        Err(e) => {
            idn_open_protocol::log::log(&format!(
                "site manager: Voice's link off (UDP {at}: {e}); handsets use the WebSocket"
            ));
            None
        }
    }
}

/// Voice's addresses from the machines' WebSocket ones: `wss://H/…` → `https://H/voice`,
/// `ws://H/…` → `http://H/voice` (QUIC ones, `idn://`, aren't HTTP).
fn voice_urls(public: &str) -> Vec<String> {
    public
        .split(',')
        .filter_map(|u| {
            let u = u.trim();
            let (scheme, rest) = match u.strip_prefix("wss://") {
                Some(r) => ("https", r),
                None => ("http", u.strip_prefix("ws://")?),
            };
            let host = rest.split('/').next().filter(|h| !h.is_empty())?;
            Some(format!("{scheme}://{host}/voice"))
        })
        .collect()
}

/// Where this machine's programs reach the HTTP port `IDN_HTTP` listens on: loopback when
/// it listens on every address, else the address it was given (a LAN address alone isn't
/// reachable on 127.0.0.1).
fn feed_host(http: &str) -> String {
    match http.trim().parse::<std::net::SocketAddr>() {
        Ok(a) if a.ip().is_unspecified() => {
            if a.is_ipv4() {
                format!("127.0.0.1:{}", a.port())
            } else {
                format!("[::1]:{}", a.port())
            }
        }
        Ok(a) => a.to_string(),
        // A name (localhost:8080): as given.
        Err(_) => http.trim().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_https_port_is_set_or_off() {
        assert_eq!(
            super::https_setting(None).map(|a| a.port()),
            Some(idn_common::ports::SITE_VOICE)
        );
        assert_eq!(super::https_setting(Some("off")), None);
        assert_eq!(
            super::https_setting(Some("29230")).map(|a| a.port()),
            Some(29230)
        );
        assert_eq!(
            super::https_setting(Some("127.0.0.1:9443")),
            Some("127.0.0.1:9443".parse().unwrap())
        );
    }

    #[test]
    fn handsets_are_told_the_public_addresses() {
        assert_eq!(
            super::voice_urls(
                "wss://hub.example.com/idn/link, idn://hub.example.com:29206,ws://10.0.0.5:8080/idn/link"
            ),
            vec![
                "https://hub.example.com/voice",
                "http://10.0.0.5:8080/voice"
            ]
        );
        assert!(super::voice_urls("").is_empty());
    }

    #[test]
    fn feeds_reach_the_address_the_port_listens_on() {
        assert_eq!(super::feed_host("0.0.0.0:8080"), "127.0.0.1:8080");
        assert_eq!(super::feed_host("[::]:9000"), "[::1]:9000");
        assert_eq!(super::feed_host("192.168.10.5:8080"), "192.168.10.5:8080");
        assert_eq!(super::feed_host("127.0.0.1:18082"), "127.0.0.1:18082");
        assert_eq!(super::feed_host("localhost:8080"), "localhost:8080");
    }
}
