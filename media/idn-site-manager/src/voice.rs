//! IDN Voice in the IDN Site Manager (spec/extensions/idn-voice.md §V.1): HTTPS on the
//! panel's port plus one (29203; `IDN_VOICE_PORT`, `off`) with the panel's certificate,
//! IDN's link for handsets on UDP at the same port number (§V.14), WebRTC on UDP 29207
//! (`IDN_VOICE_UDP`), the site's users, audit and MQTT; a loopback port for IEM feeds.

use crate::State;
use idn_open_protocol::web::Role;
use idn_voice::{Host, User, Voice, VoiceConfig};
use std::sync::Arc;

/// Voice's view of the Site Manager: weak, so Voice's tasks never hold it up. (Holding it,
/// the last of it could go on Voice's own threads as Voice shut down, and with it the
/// MQTT broker's runtime: tokio panicked there, "Cannot drop a runtime in a context where
/// blocking is not allowed", with several Site Managers in one process.)
struct MediaHost {
    state: std::sync::Weak<State>,
}

fn user(name: &str, role: Role) -> User {
    User {
        name: name.to_owned(),
        roles: vec![role.as_str().to_owned()],
        admin: role == Role::Admin,
    }
}

impl Host for MediaHost {
    fn session_user(&self, h: &axum::http::HeaderMap) -> Option<User> {
        let state = self.state.upgrade()?;
        // The panel's session (its cookie, SameSite=Strict, comes with requests to this
        // port too: same host).
        let id = idn_open_protocol::web::cookie(h, "idn_site")?;
        let name = state.sessions.user(Some(id))?;
        let role = state.users.role(&name)?;
        Some(user(&name, role))
    }

    fn login(&self, name: &str, password: &str, _from: std::net::IpAddr) -> Option<User> {
        let state = self.state.upgrade()?;
        state
            .users
            .verify(name, password, state.password().as_deref())
            .map(|(n, r)| user(&n, r))
    }

    fn users(&self) -> Vec<User> {
        let Some(state) = self.state.upgrade() else {
            return Vec::new();
        };
        let mut list: Vec<User> = state
            .users
            .list()
            .into_iter()
            .map(|(n, r)| user(&n, r))
            .collect();
        if !list.iter().any(|u| u.name == "admin") {
            list.push(user("admin", Role::Admin));
        }
        list
    }

    fn audit(&self, who: &str, action: &str, detail: serde_json::Value) {
        if let Some(state) = self.state.upgrade() {
            state.users.audit(who, "", action, &detail, None);
        }
    }

    fn publish(&self, topic: &str, body: &serde_json::Value, retain: bool) {
        if let Some(state) = self.state.upgrade()
            && let Some(m) = &state.mqtt
        {
            let msg = idn_mqtt::Msg::new(
                &format!("idn/site/voice/{topic}"),
                body.to_string().into_bytes(),
                1,
                retain,
            );
            let _ = m.broker().publish(&msg);
        }
    }
}

/// Voice running beside the panel: its runtime (dropped with the panel) and its HTTPS port.
pub(crate) struct Running {
    /// Voice over IDN's link (§V.14), if its UDP port was free: stops when dropped.
    _link: Option<idn_voice::VoiceLink>,
    /// Held: Voice's tasks run on it and end with it.
    _rt: crate::OwnedRuntime,
    pub port: u16,
}

/// Starts Voice for the panel on `panel_port` + 1; `None` if turned off or it can't start
/// (logged).
/// `fingerprint`: the SHA-256 of `tls`'s certificate, which handsets pin (§V.13).
pub(crate) fn start(
    state: &Arc<State>,
    tls: Arc<rustls::ServerConfig>,
    fingerprint: &str,
    panel_port: u16,
) -> Option<Running> {
    // (Off as every switch says it; a port that isn't one logged and the default kept;
    // F8-6.)
    let port = match idn_common::switches::value("IDN_VOICE_PORT")
        .as_deref()
        .map(str::trim)
    {
        Some(p) if idn_common::switches::says_off(p) => return None,
        Some(p) if !p.is_empty() => match p.parse::<u16>() {
            Ok(p) if p > 0 => p,
            _ => {
                idn_open_protocol::log::log(&format!(
                    "IDN_VOICE_PORT={p:?}: not a port; the panel's + 1 used"
                ));
                panel_port.checked_add(1)?
            }
        },
        _ => panel_port.checked_add(1)?,
    };
    let r = (|| -> std::io::Result<Running> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("idn-voice")
            .enable_all()
            .build()?;
        let mut cfg = VoiceConfig::new(state.dir.join("voice"));
        cfg.udp = idn_voice::udp_setting(
            idn_common::switches::value("IDN_VOICE_UDP").as_deref(),
            cfg.udp,
        );
        // The API and its WebSocket only: IDN Media has no UI of its own.
        cfg.pages = false;
        // The certificate it serves (the panel's): told to handsets to pin (§V.13).
        if let Some(fp) = idn_voice::model::fingerprint_text(fingerprint) {
            cfg.cert_sha256 = Some(fp);
        }
        // (IDN's programs folder: a host program's own folder holds no idn-codec.)
        cfg.codec = idn_common::supervisor::programs_dir().and_then(|d| {
            let p = d.join(if cfg!(windows) {
                "idn-codec.exe"
            } else {
                "idn-codec"
            });
            p.exists().then_some(p)
        });
        let local = std::net::TcpListener::bind("127.0.0.1:0")?;
        cfg.feed_url = Some(format!("ws://{}/voice/ws", local.local_addr()?));
        // (Bound first: the link takes the same port number on UDP.)
        let l = std::net::TcpListener::bind(("0.0.0.0", port))?;
        let port = l.local_addr()?.port();
        let v = {
            let _g = rt.enter();
            Voice::start(
                cfg,
                Arc::new(MediaHost {
                    state: Arc::downgrade(state),
                }),
            )?
        };
        v.serve_tls(l, tls, rt.handle())?;
        let link = serve_link(&v, port, rt.handle());
        // The site broker's Voice commands (cues, §V.10); weak: the panel outlives nothing.
        if let Some(m) = &state.mqtt {
            let hub = Arc::downgrade(&v.hub);
            m.on_voice(Box::new(move |topic, payload| {
                if let Some(h) = hub.upgrade() {
                    h.mqtt_command(topic, payload);
                }
            }));
        }
        v.serve_local(local, rt.handle())?;
        Ok(Running {
            _link: link,
            _rt: crate::OwnedRuntime::new(rt),
            port,
        })
    })();
    match r {
        Ok(r) => Some(r),
        Err(e) => {
            idn_open_protocol::log::log(&format!("site manager: Voice off: {e}"));
            None
        }
    }
}

/// Voice over IDN's link (§V.14) on UDP `port` (the HTTPS port's number, as the link
/// takes its TCP port's), with a link key of Voice's own (`voice/link.key`: the site's
/// backup carries it, so handsets' pins hold); `None` if it can't (logged: handsets then
/// use the WebSocket).
fn serve_link(v: &Voice, port: u16, rt: &tokio::runtime::Handle) -> Option<idn_voice::VoiceLink> {
    let at = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    let r = idn_open_protocol::link::LinkConfig::in_dir(&v.cfg.dir)
        .and_then(|link| v.serve_link(at, &link, rt));
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
                "site manager: Voice's link off (UDP {port}: {e}); handsets use the WebSocket"
            ));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Running;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
    }

    /// Voice dropped inside async code (a host's `Handle` dropped in a task, or while a
    /// `#[tokio::test]` unwinds) ends without a panic, as from a plain
    /// thread.
    #[test]
    fn dropped_inside_async_code_without_a_panic() {
        let outer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let r = Running {
            _link: None,
            _rt: crate::OwnedRuntime::new(runtime()),
            port: 0,
        };
        outer.block_on(async move { drop(r) });
        drop(Running {
            _link: None,
            _rt: crate::OwnedRuntime::new(runtime()),
            port: 0,
        });
    }
}
