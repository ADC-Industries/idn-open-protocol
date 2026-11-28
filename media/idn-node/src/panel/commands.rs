//! The panel's changes (`POST /api/v1/<command>`), by name, run through the shared API
//! front (`web::engine`: the change header, per-control sequence numbers, a read-only
//! caller refused, the audit); see panel/front.rs.

use super::*;

/// The commands this panel takes (its schema; a test checks each has its branch).
pub(super) const COMMANDS: &[&str] = &[
    "settings",
    "route",
    "receiver_latency",
    "service",
    "vbcable/install",
    "vscreens",
    "drivers/install",
    "update/check",
    "apps/find",
    "aes67/find",
    "sites/search",
    "retry",
    "pairs/find",
    "pairs/request",
    "pairs/confirm",
    "pairs/answer",
    "site/create",
    "site/join",
    "site/leave",
    "site/confirm",
    "pairs/remove",
];

impl State {
    /// Command `cmd` with its JSON `body`, from a session opened by a program (`limited`:
    /// some changes refused) or on the network (`lan`).
    #[allow(unused_variables)]
    pub(super) fn command(&self, cmd: &str, body: &[u8], limited: bool, lan: bool) -> Response {
        match cmd {
            "settings" => match self.settings(body, limited) {
                Ok(()) => Response::json(r#"{"ok":true}"#),
                Err(e) => Response::json_err("400 Bad Request", &e),
            },
            // Routes of this machine's receivers (idn-routing.md §A.3).
            "route" => match self.set_routes(body) {
                Ok(()) => Response::json(r#"{"ok":true}"#),
                Err(e) => Response::json_err("400 Bad Request", &e),
            },
            // A receiver's latency (idn-routing.md §A.12): `ms` a number, or null for the
            // program's own.
            "receiver_latency" => {
                let v: Value = serde_json::from_slice(body).unwrap_or_default();
                match latency_of(&v) {
                    Ok(ms) => match self.core.set_receiver_latency(
                        v["receiver"].as_str().unwrap_or(""),
                        ms,
                        false,
                    ) {
                        Ok(()) => Response::json(r#"{"ok":true}"#),
                        Err(e) => Response::json_err("400 Bad Request", &e.to_string()),
                    },
                    Err(e) => Response::json_err("400 Bad Request", e),
                }
            }
            // A service switched on or off, keeping its settings (§N.14.4).
            "service" => {
                // `on`: running now (until the Node starts again); `auto`: whether it
                // starts by itself (the Services page's two switches).
                let v: Value = serde_json::from_slice(body).unwrap_or_default();
                let id = v.get("id").and_then(Value::as_str).unwrap_or("");
                let (on, auto) = (
                    v.get("on").and_then(Value::as_bool),
                    v.get("auto").and_then(Value::as_bool),
                );
                if id.is_empty() || (on.is_none() && auto.is_none()) {
                    return Response::json_err("400 Bad Request", "give id, and on or auto");
                }
                match self.core.set(Change {
                    service_now: on.map(|on| (id.to_owned(), on)),
                    service_auto: auto.map(|a| (id.to_owned(), a)),
                    ..Default::default()
                }) {
                    Ok(()) => Response::json(r#"{"ok":true}"#),
                    Err(e) => Response::json_err("400 Bad Request", &e.to_string()),
                }
            }
            // "Programs' sound" is showing: the programs with sound open, asked of the
            // session runner for the next minute.
            // "Install VB-CABLE" (Windows): the session agent runs VB-Audio's setup in the session.
            // (These three change the computer itself: refused on a machine the site has
            // locked, §N.31.4, as every other change made here.)
            "vbcable/install" | "vscreens" | "drivers/install" if self.core.site_locked() => {
                Response::json_err(
                    "403 Forbidden",
                    "this machine is locked by the site: change it from the Site Manager",
                )
            }
            "vbcable/install" => {
                self.core.request_vbcable();
                Response::json(r#"{"ok":true}"#)
            }
            // A virtual screen added or removed (§N.36): on Windows here and now (the
            // service is LocalSystem); on Linux left for the root helper, done at the next
            // start of the computer.
            "vscreens" => {
                let v: Value = serde_json::from_slice(body).unwrap_or_default();
                let s = |k: &str| v.get(k).and_then(Value::as_str).filter(|x| !x.is_empty());
                let changed = vscreens_change(s("action").unwrap_or(""), s("mode"), s("connector"));
                self.devices_changed();
                match changed {
                    Ok(msg) => Response::json(json!({ "ok": true, "message": msg }).to_string()),
                    Err(e) => Response::json_err("400 Bad Request", &e.to_string()),
                }
            }
            // A bundled driver installed (Windows: IDN's virtual sound card).
            "drivers/install" => {
                let v: Value = serde_json::from_slice(body).unwrap_or_default();
                let key = v.get("driver").and_then(Value::as_str).unwrap_or("");
                let Some(d) = crate::drivers::by_key(key) else {
                    return Response::json_err("400 Bad Request", "driver: display or audio");
                };
                let installed = crate::drivers::win::install(d);
                self.devices_changed();
                match installed {
                    Ok(reboot) => {
                        Response::json(json!({ "ok": true, "reboot": reboot }).to_string())
                    }
                    Err(e) => Response::json_err("400 Bad Request", &e.to_string()),
                }
            }
            // The programs' logs (§N.42): the list, or one's last lines.
            "update/check" => match self.core.check_updates() {
                Ok(()) => Response::json(r#"{"ok":true}"#),
                Err(e) => Response::json_err("400 Bad Request", &e),
            },
            "apps/find" => {
                self.core.want_audio_apps();
                Response::json(r#"{"ok":true}"#)
            }
            "aes67/find" => {
                self.find_aes67();
                Response::json(r#"{"ok":true}"#)
            }
            // The wizard's "look again" (§N.22).
            "sites/search" => {
                self.core.search_sites_now();
                Response::json(r#"{"ok":true}"#)
            }
            "retry" => {
                self.core.retry();
                Response::json(r#"{"ok":true}"#)
            }
            // Linked computers (§N.32): a window showing a PIN, linking to a machine that
            // shows one, and forgetting a link.
            "pairs/find" => {
                let found: Vec<Value> = self
                    .core
                    .pair_find()
                    .iter()
                    .map(
                        |f| json!({"name": f.instance, "addr": f.addr.to_string(), "port": f.port}),
                    )
                    .collect();
                Response::json(json!({"ok": true, "machines": found}).to_string())
            }
            // Asks another computer to link: the code both show; its answer comes in the
            // status (`pairs.outgoing`).
            "pairs/request" => {
                let v: Value = serde_json::from_slice(body).unwrap_or_default();
                // An address and port (found on the network), or `host` typed in: a name
                // or an address, `:port` only when not the usual one.
                let addr = match (v["addr"].as_str(), v["port"].as_u64(), v["host"].as_str()) {
                    (Some(a), Some(p), _) => format!("{a}:{p}").parse().ok(),
                    (_, _, Some(h)) => {
                        use std::net::ToSocketAddrs;
                        let h = h.trim();
                        let with_port = if h
                            .rsplit_once(':')
                            .is_some_and(|(_, p)| p.parse::<u16>().is_ok())
                        {
                            h.to_owned()
                        } else {
                            format!("{h}:{}", crate::pairs::LINK_PORT)
                        };
                        with_port
                            .to_socket_addrs()
                            .ok()
                            .and_then(|mut a| a.find(|x| x.is_ipv4()))
                    }
                    _ => None,
                };
                match addr {
                    Some(a) => match self.core.pair_ask(a) {
                        Ok((name, code)) => Response::json(
                            json!({"ok": true, "name": name, "code": code}).to_string(),
                        ),
                        Err(e) => Response::json_err("400 Bad Request", &e),
                    },
                    None => Response::json_err(
                        "400 Bad Request",
                        "the computer to ask: its name or address (not found)",
                    ),
                }
            }
            // This computer's word on the request it made: `{"ok": true}` the codes match,
            // `false` cancel.
            "pairs/confirm" => {
                let v: Value = serde_json::from_slice(body).unwrap_or_default();
                match self
                    .core
                    .pair_confirm(v["ok"].as_bool().unwrap_or(false), v["code"].as_str())
                {
                    Ok(()) => Response::json(r#"{"ok":true}"#),
                    Err(e) => Response::json_err("400 Bad Request", &e),
                }
            }
            // This computer's answer to a request waiting: accept (remembered or this time
            // only) or reject.
            "pairs/answer" => {
                let v: Value = serde_json::from_slice(body).unwrap_or_default();
                match self.core.pair_answer(
                    v["accept"].as_bool().unwrap_or(false),
                    v["remember"].as_bool().unwrap_or(false),
                    v["code"].as_str(),
                ) {
                    Ok(()) => Response::json(r#"{"ok":true}"#),
                    Err(e) => Response::json_err("400 Bad Request", &e),
                }
            }
            // A new site on this machine (the appliance's first start, §N.11.7):
            // `{"password"}`, its Site Manager's site password.
            // (Neither from a panel a program opened: it set the site and the site
            // password, which such a panel can't change otherwise; bug run 4.)
            "site/create" | "site/join" if limited => Response::json_err(
                "403 Forbidden",
                "this panel was opened by a program: the site is changed from the Node's own panel",
            ),
            "site/create" => {
                let v: Value = serde_json::from_slice(body).unwrap_or_default();
                match self.core.create_site(v["password"].as_str().unwrap_or("")) {
                    Ok(()) => Response::json(r#"{"ok":true}"#),
                    Err(e) => Response::json_err("400 Bad Request", &e.to_string()),
                }
            }
            // A join code from the site's Site Manager (§N.11.8): `{"code": "482 913"}`.
            "site/join" => {
                let v: Value = serde_json::from_slice(body).unwrap_or_default();
                match self.core.join_code(v["code"].as_str().unwrap_or("")) {
                    Ok(()) => Response::json(r#"{"ok":true}"#),
                    Err(e) => Response::json_err("400 Bad Request", &e),
                }
            }
            // Leaves the site (§N.42): its certificates forgotten, no site after.
            // (Not from a panel a program opened: the site is changed from the Node's own.)
            "site/leave" => {
                if limited {
                    return Response::json_err(
                        "403 Forbidden",
                        "this panel was opened by a program: leave the site from the IDN Node's own panel",
                    );
                }
                match self.core.leave_site() {
                    Ok(()) => Response::json(r#"{"ok":true}"#),
                    Err(e) => Response::json_err("400 Bad Request", &e),
                }
            }
            // Joining a site by code: the Site Manager shows the same code, or not.
            "site/confirm" => {
                let v: Value = serde_json::from_slice(body).unwrap_or_default();
                match self.core.enrollment() {
                    Some(e) => {
                        e.confirm_code(v["matches"].as_bool().unwrap_or(false));
                        Response::json(r#"{"ok":true}"#)
                    }
                    None => Response::json_err("400 Bad Request", "no site being joined"),
                }
            }
            "pairs/remove" => {
                let v: Value = serde_json::from_slice(body).unwrap_or_default();
                match self.core.pair_remove(v["fp"].as_str().unwrap_or("")) {
                    Ok(()) => Response::json(r#"{"ok":true}"#),
                    Err(e) => Response::json_err("400 Bad Request", &e),
                }
            }
            _ => Response::json_err("404 Not Found", &format!("no command {cmd:?}")),
        }
    }
}
