//! The Site Manager's commands (`POST /api/v1/<command>`), by name: run for a user with
//! a role, through the shared API front (`crate::front`: the change header, sequence
//! numbers, the role and the audit checked there), the push channel and a host's typed
//! calls alike. What needs the request itself (logging in and out, a backup's file, a
//! restore, a release's pieces) is in requests.rs.

use super::*;

impl State {
    /// Command `cmd` with its JSON `body` for `user` (with `role`): the answer, as
    /// over HTTP.
    pub(super) fn command(
        &self,
        cmd: &str,
        body: &[u8],
        user: &str,
        role: Option<users::Role>,
    ) -> Response {
        match cmd {
            "release/done" => match self.updates.upload_done() {
                Ok(m) => {
                    self.log(&format!("panel: release {} kept", m.version));
                    Response::json(
                        serde_json::json!({"ok": true, "version": m.version}).to_string(),
                    )
                }
                Err(e) => Response::json_err("400 Bad Request", &e.to_string()),
            },
            "release/delete" => {
                match self
                    .updates
                    .delete(&field(body, "version").unwrap_or_default())
                {
                    Ok(()) => ok(),
                    Err(e) => Response::json_err("400 Bad Request", &e.to_string()),
                }
            }
            "update" => {
                let v: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
                let to: Vec<String> = v["to"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
                match self.updates.start(
                    &self.authority,
                    v["version"].as_str().unwrap_or(""),
                    to,
                    v["when"].as_str().unwrap_or("now"),
                    v["staged"].as_bool().unwrap_or(true),
                    self.port,
                ) {
                    Ok(j) => {
                        self.log(&format!(
                            "panel: update {} sent ({})",
                            v["version"].as_str().unwrap_or(""),
                            j["state"].as_str().unwrap_or("")
                        ));
                        Response::json(j.to_string())
                    }
                    Err(e) => Response::json_err("400 Bad Request", &e),
                }
            }
            "update/stop" => {
                self.updates.stop();
                ok()
            }
            "rollback" => {
                let fp = field(body, "fingerprint").unwrap_or_default();
                match updates::rollback(&self.authority, &fp) {
                    Ok(()) => {
                        self.log(&format!(
                            "panel: rollback asked of {}",
                            fp.get(..16).unwrap_or(&fp)
                        ));
                        ok()
                    }
                    Err(e) => Response::json_err("400 Bad Request", &e),
                }
            }
            "apply" => match self.apply(body) {
                Ok(v) => Response::json(v.to_string()),
                Err(e) => Response::json_err("400 Bad Request", &e),
            },
            "apply_template" => match self.apply_template(body) {
                Ok(v) => Response::json(v.to_string()),
                Err(e) => Response::json_err("400 Bad Request", &e),
            },
            "copy_settings" => match self.copy_settings(body) {
                Ok(v) => Response::json(v.to_string()),
                Err(e) => Response::json_err("400 Bad Request", &e),
            },
            // One cell of the matrix (crate::matrix): the change it means, made.
            "route_cell" => match self.route_cell(body) {
                Ok(v) => Response::json(v.to_string()),
                Err(e) => Response::json_err("400 Bad Request", &e),
            },
            // A change to a network device answered with what was done: for a setting
            // (R.32 stage 2), the value the device reads back (K.14).
            "network-devices/do" => {
                let v: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
                match netaudio::do_request(&self.authority, &v) {
                    Ok(did) => {
                        self.log(&format!("panel: network audio: {did}"));
                        Response::json(serde_json::json!({"ok": true, "did": did}).to_string())
                    }
                    Err(e) => Response::json_err("400 Bad Request", &e),
                }
            }
            // The site's MQTT broker (§N.34): on or off, its address; logins.
            "mqtt" => {
                let Some(m) = &self.mqtt else {
                    return Response::json_err(
                        "503 Service Unavailable",
                        "the MQTT broker couldn't open its store",
                    );
                };
                let v: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
                // Either left out: as it is now (only `listen` sent turned the broker off:
                // bug run O).
                let now = m.settings();
                let on = v["on"]
                    .as_bool()
                    .unwrap_or_else(|| now["on"].as_bool().unwrap_or(false));
                let listen = v["listen"]
                    .as_str()
                    .or_else(|| now["listen"].as_str())
                    .unwrap_or("")
                    .to_owned();
                match m.set(on, &listen) {
                    Ok(()) => {
                        self.log(&format!(
                            "panel: site MQTT {}",
                            if on { "on" } else { "off" }
                        ));
                        ok()
                    }
                    Err(e) => Response::json_err("400 Bad Request", &e.to_string()),
                }
            }
            "mqtt_user" => {
                let Some(m) = &self.mqtt else {
                    return Response::json_err(
                        "503 Service Unavailable",
                        "the MQTT broker couldn't open its store",
                    );
                };
                let v: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
                let name = v["username"].as_str().unwrap_or("").trim();
                let r = if v["remove"] == true {
                    m.remove_user(name).map(drop)
                } else {
                    m.set_user(name, v["password"].as_str().unwrap_or(""))
                };
                match r {
                    Ok(()) => ok(),
                    Err(e) => Response::json_err("400 Bad Request", &e.to_string()),
                }
            }
            // The metrics token (N5): a new one (shown once, kept as its hash) or none.
            "metrics_token" => {
                let v: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
                let path = self.dir.join(METRICS_TOKEN);
                match v["action"].as_str().unwrap_or("") {
                    "new" => {
                        let token = idn_open_protocol::random::token_hex(24);
                        let hash = idn_open_protocol::hash::sha256_hex(token.as_bytes());
                        match idn_open_protocol::trust::pki::write_private(&path, hash.as_bytes()) {
                            Ok(()) => {
                                self.log("panel: a new metrics token");
                                Response::json(
                                    serde_json::json!({"ok": true, "token": token}).to_string(),
                                )
                            }
                            Err(e) => {
                                Response::json_err("500 Internal Server Error", &e.to_string())
                            }
                        }
                    }
                    "off" => {
                        let _ = std::fs::remove_file(&path);
                        self.log("panel: metrics off");
                        ok()
                    }
                    _ => Response::json_err("400 Bad Request", "action: new or off"),
                }
            }
            // Routing presets (N3, idn-routing.md §A.11).
            "routing_preset" => {
                let v: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
                let name = v["name"].as_str().unwrap_or("").trim().to_owned();
                match v["action"].as_str().unwrap_or("") {
                    "save" => match self.save_preset(&name) {
                        Ok(n) => Response::json(
                            serde_json::json!({"ok": true, "receivers": n}).to_string(),
                        ),
                        Err(e) => Response::json_err("400 Bad Request", &e),
                    },
                    "recall" => match self.recall_preset(&name) {
                        Ok(v) => Response::json(v.to_string()),
                        Err(e) => Response::json_err("400 Bad Request", &e),
                    },
                    "delete" => match self.delete_preset(&name) {
                        Ok(()) => ok(),
                        Err(e) => Response::json_err("400 Bad Request", &e),
                    },
                    _ => Response::json_err("400 Bad Request", "action: save, recall or delete"),
                }
            }
            "template" => {
                let v: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
                let settings =
                    (!v["delete"].as_bool().unwrap_or(false)).then(|| v["settings"].clone());
                match self.save_template(v["name"].as_str().unwrap_or(""), settings) {
                    Ok(()) => ok(),
                    Err(e) => Response::json_err("400 Bad Request", &e),
                }
            }
            // A join code for one machine (extensions §E.10.10): shown this once, then
            // listed in the status by its expiry and tries only. `{"cancel": id}` takes a
            // waiting one back.
            "join_code" => {
                let v: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
                if let Some(id) = v["cancel"].as_u64() {
                    return match self.authority.cancel_join_code(id) {
                        Ok(()) => ok(),
                        Err(e) => Response::json_err("400 Bad Request", &e.to_string()),
                    };
                }
                match self.authority.issue_join_code(user) {
                    Ok(c) => {
                        self.log(&format!(
                            "panel: join code {} issued by {user}, good for 10 minutes",
                            c.id
                        ));
                        Response::json(
                            serde_json::json!({"ok": true, "id": c.id, "code": c.code,
                                "shown": idn_open_protocol::trust::show_code(&c.code),
                                "expires": c.expires})
                            .to_string(),
                        )
                    }
                    Err(e) => Response::json_err("500 Internal Server Error", &e.to_string()),
                }
            }
            // Named, long-lived API keys (admins).
            "api_key" => {
                let v: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
                match self.api_key_new(&v, user) {
                    Ok(a) => Response::json(a.to_string()),
                    Err(e) => Response::json_err("400 Bad Request", &e),
                }
            }
            "api_key_revoke" => {
                let v: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
                match self.api_key_revoke(&v) {
                    Ok(()) => ok(),
                    Err(e) => Response::json_err("400 Bad Request", &e),
                }
            }
            // Scheduled backups (§N.42): the schedule, and one now.
            "backup_schedule" => {
                let v: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
                match self.backup_schedule(&v) {
                    Ok(()) => ok(),
                    Err(e) => Response::json_err("400 Bad Request", &e),
                }
            }
            "backup_now" => match self.backup_now() {
                Ok(name) => {
                    Response::json(serde_json::json!({"ok": true, "file": name}).to_string())
                }
                Err(e) => Response::json_err("400 Bad Request", &e),
            },
            // A token for an engine (§N.31.5), for this user, as their role allows.
            "token" => self.token_request(body, user, role.unwrap_or(users::Role::Viewer)),
            // Users (§N.31): added, changed or removed; their sessions end.
            "user" => {
                let v: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
                let name = v["name"].as_str().unwrap_or("").trim().to_owned();
                // (A new user has no tokens to withdraw.)
                let existed = self.users.role(&name).is_some();
                let res = match v["action"].as_str().unwrap_or("") {
                    // No role: the user's own kept (a password reset doesn't demote), a
                    // new user a viewer (bug run O).
                    "set" => match v["role"].as_str().filter(|r| !r.is_empty()) {
                        Some(r) if users::Role::parse(r).is_none() => {
                            Err("role: viewer, operator or admin".into())
                        }
                        r => self
                            .users
                            .set(
                                &name,
                                v["password"].as_str(),
                                r.and_then(users::Role::parse),
                            )
                            .map_err(|e| e.to_string()),
                    },
                    "remove" => match self.users.remove(&name) {
                        Ok(true) => Ok(()),
                        Ok(false) => Err(format!("no user {name:?}")),
                        Err(e) => Err(e.to_string()),
                    },
                    _ => Err("action: set or remove".into()),
                };
                match res {
                    Ok(()) => {
                        self.sessions.end_user(&name);
                        // Their engine tokens too, on every machine (§N.31.5).
                        if existed {
                            self.withdraw_tokens(&name);
                        }
                        self.log(&format!("panel: user {name} changed by {user}"));
                        ok()
                    }
                    Err(e) => Response::json_err("400 Bad Request", &e),
                }
            }
            _ => match self.change(cmd, body) {
                Ok(()) => ok(),
                Err(e) => Response::json_err("400 Bad Request", &e),
            },
        }
    }

    /// `POST /api/v1/route_cell`: what a click on the cell means ([`crate::matrix::cell_action`]),
    /// made through the change it maps to: `{"ok": true, "did": path, "body"}`, or `{"ok":
    /// true, "did": "nothing", "why"}` when it is so already.
    pub(super) fn route_cell(&self, b: &[u8]) -> Result<serde_json::Value, String> {
        let v: serde_json::Value = serde_json::from_slice(b).map_err(|e| e.to_string())?;
        let m = crate::matrix::build(&self.authority.node_reports(), &self.ds_sources());
        match crate::matrix::cell_action(&m, &v)? {
            crate::matrix::CellAction::Nothing(why) => {
                Ok(serde_json::json!({"ok": true, "did": "nothing", "why": why}))
            }
            crate::matrix::CellAction::Post(path, body) => {
                self.change(path, body.to_string().as_bytes())?;
                Ok(serde_json::json!({
                    "ok": true,
                    "did": path.trim_start_matches("/api/v1/"),
                    "body": body,
                }))
            }
        }
    }

    /// A change from the panel, by its command's name (`route`; a path, `/api/v1/route`, as
    /// the matrix's cells give it, is taken too).
    pub(super) fn change(&self, cmd: &str, b: &[u8]) -> Result<(), String> {
        let cmd = crate::requests::action_of(cmd);
        let a = &self.authority;
        let s = |k: &str| field(b, k).unwrap_or_default();
        let r: io::Result<()> = match cmd {
            "pairing" => {
                let secs = s("seconds").parse::<u64>().unwrap_or(600).clamp(10, 3600);
                a.open_pairing(Duration::from_secs(secs));
                self.log(&format!("panel: pairing window open for {secs} s"));
                Ok(())
            }
            "close" => {
                a.close_pairing();
                Ok(())
            }
            "approve" => (|| -> io::Result<()> {
                let id: u64 = s("id")
                    .parse()
                    .map_err(|_| io::Error::other("no request"))?;
                // Named as it is approved (a client's Site page): the name sent to the machine
                // once it is in.
                // (Cleaned as the certificate's: the Node gets the same name; bug run
                // 2026-11-24.)
                let named = field(b, "name")
                    .map(|n| idn_open_protocol::trust::Authority::clean_name(&n))
                    .filter(|n| !n.is_empty());
                let fp = a
                    .pending()
                    .into_iter()
                    .find(|p| p.id == id)
                    .map(|p| p.fingerprint);
                // The certificate carries the name (the site's records and the machine's
                // own); its Node, if it has one, is named too.
                a.approve_as(id, named.as_deref())?;
                if let (Some(name), Some(fp)) = (named, fp) {
                    let a2 = a.clone();
                    std::thread::spawn(move || {
                        // (It joins first: a few tries while it connects.)
                        for _ in 0..10 {
                            std::thread::sleep(Duration::from_secs(1));
                            if let Ok(r) =
                                a2.set_node(&fp, &[("name", name.as_str())], Duration::from_secs(5))
                                && r.attr("ok") == Some("true")
                            {
                                idn_open_protocol::log::log(&format!(
                                    "site manager: {} named {name:?}",
                                    fp.get(..16).unwrap_or(&fp)
                                ));
                                return;
                            }
                        }
                    });
                }
                Ok(())
            })(),
            "deny" => s("id")
                .parse()
                .map_err(|_| io::Error::other("no request"))
                .and_then(|id| a.deny(id)),
            "revoke" => a.revoke(&s("fingerprint")),
            // A pairing relay, or not.
            "relay" => a.set_relay(&s("fingerprint"), field_bool(b, "on").unwrap_or(false)),
            // A service on a machine restarted, or the machine identified on its screens
            // (a client's Site page).
            "restart" | "identify" => {
                let fp = s("fingerprint");
                let (k, v) = if cmd == "restart" {
                    let svc = s("service");
                    if svc.is_empty() {
                        return Err("service: the service to restart".into());
                    }
                    ("restart", svc)
                } else {
                    let secs = field(b, "seconds")
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .unwrap_or(10)
                        .clamp(1, 60);
                    ("identify", secs.to_string())
                };
                match a.set_node(&fp, &[(k, v.as_str())], Duration::from_secs(5)) {
                    Ok(r) if r.attr("ok") == Some("true") => Ok(()),
                    Ok(r) => Err(io::Error::other(
                        r.attr("error").unwrap_or("refused").to_owned(),
                    )),
                    Err(e) => Err(e),
                }
            }
            // A machine's logs, asked for now (idn-node.md §N.16).
            "logs" => {
                let fp = s("fingerprint");
                a.collect_logs(&fp, Duration::from_secs(20)).map(|snap| {
                    self.log(&format!("panel: logs collected from {fp} ({snap})"));
                })
            }
            "clear" => a.clear_denied(&s("fingerprint")),
            "node" => {
                let fp = s("fingerprint");
                let mut attrs: Vec<(String, String)> = Vec::new();
                for k in ["name", "interface"] {
                    if let Some(v) = field(b, k) {
                        attrs.push((k.to_owned(), v.trim().to_owned()));
                    }
                }
                if let Some(v) = field_bool(b, "require") {
                    attrs.push(("require".into(), v.to_string()));
                }
                // Locked to the site (N4, §N.31): settings and routes from here only.
                if let Some(v) = field_bool(b, "site_lock") {
                    attrs.push(("site_lock".into(), v.to_string()));
                }
                // The machine's video settings (idn-node.md §N.13); "" unsets one.
                for k in idn_open_protocol::node::video::KEYS {
                    if let Some(v) = field(b, &format!("video_{k}")) {
                        let mut check = idn_open_protocol::node::video::VideoSettings::default();
                        check.set(k, &v).map_err(|e| e.to_string())?;
                        attrs.push((format!("video.{k}"), v.trim().to_owned()));
                    }
                }
                // The site clock (idn-clock.md §C.5): on/off, domain, priority1 ("" automatic).
                if let Some(v) = field_bool(b, "clock") {
                    attrs.push(("clock".into(), if v { "on" } else { "off" }.into()));
                }
                if let Some(v) = field(b, "clock_protocol") {
                    let v = v.trim().to_owned();
                    if !v.is_empty() && idn_open_protocol::ptp::Protocol::parse(&v).is_none() {
                        return Err("clock_protocol: v2, v1 or both".into());
                    }
                    attrs.push(("clock_protocol".into(), v));
                }
                for k in ["clock_domain", "clock_priority"] {
                    if let Some(v) = field(b, k) {
                        let v = v.trim().to_owned();
                        if !v.is_empty() && v.parse::<u8>().is_err() {
                            return Err(format!("{k}: a number from 0 to 255"));
                        }
                        attrs.push((k.into(), v));
                    }
                }
                // The panels' simple-mode PIN (idn-node.md §N.20): set, wiped, the site's.
                // "none" is no PIN, as apply, copy and templates take it (bug run O); empty
                // is unchanged.
                let pin = field(b, "pin")
                    .map(|p| p.trim().to_owned())
                    .filter(|p| !p.is_empty());
                let pin_none = pin
                    .as_deref()
                    .is_some_and(|p| p.eq_ignore_ascii_case("none"));
                if field_bool(b, "pin_off") == Some(true) || pin_none {
                    attrs.push(("panel_pin".into(), String::new()));
                } else if let Some(p) = pin {
                    if !(4..=32).contains(&p.chars().count()) {
                        return Err("the PIN: 4 to 32 characters".into());
                    }
                    attrs.push((
                        "panel_pin".into(),
                        web::hash_password(&p).map_err(|e| e.to_string())?,
                    ));
                }
                if let Some(v) = field_bool(b, "pin_site_only") {
                    attrs.push(("panel_site_only".into(), v.to_string()));
                }
                if field_bool(b, "lan_off") == Some(true) {
                    attrs.push(("lan_password".into(), String::new()));
                } else if let Some(p) = field(b, "lan_password").filter(|p| !p.is_empty()) {
                    attrs.push((
                        "lan_password".into(),
                        web::hash_password(&p).map_err(|e| e.to_string())?,
                    ));
                }
                let refs: Vec<(&str, &str)> = attrs
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect();
                match a.set_node(&fp, &refs, Duration::from_secs(5)) {
                    Ok(reply) if reply.attr("ok") == Some("true") => {
                        self.log(&format!(
                            "panel: settings sent to {}",
                            fp.get(..16).unwrap_or(&fp)
                        ));
                        Ok(())
                    }
                    Ok(reply) => Err(io::Error::other(
                        reply
                            .attr("error")
                            .unwrap_or("the machine refused")
                            .to_owned(),
                    )),
                    Err(e) => Err(e),
                }
            }
            // A receiver's latency on one machine (idn-routing.md §A.12).
            "receiver_latency" => {
                let v: serde_json::Value = serde_json::from_slice(b).map_err(|e| e.to_string())?;
                let ms = latency_of(&v)?;
                let (fp, receiver) = (s("fingerprint"), s("receiver"));
                match a.latency_node(&fp, &receiver, ms, Duration::from_secs(5)) {
                    Ok(reply) if reply.attr("ok") == Some("true") => {
                        self.log(&format!(
                            "panel: latency of {receiver} on {}: {}",
                            fp.get(..16).unwrap_or(&fp),
                            ms.map_or("the program's own".into(), |m| format!("{m} ms"))
                        ));
                        Ok(())
                    }
                    Ok(reply) => Err(io::Error::other(
                        reply
                            .attr("error")
                            .unwrap_or("the machine refused")
                            .to_owned(),
                    )),
                    Err(e) => Err(e),
                }
            }
            // Audio routes on one machine (idn-routing.md §A.4, §A.6).
            // A change to the network's devices (roadmap R.32 stage 1b): a subscription set
            // or cleared, a device or channel renamed; made by the Node of a machine that
            // reads that device (or the one asked), audited as every change here.
            // The pool (R.32 stage 1c): a route between a network device and IDN.
            "network-devices/pool" => {
                let v: serde_json::Value = serde_json::from_slice(b).map_err(|e| e.to_string())?;
                let did = netaudio_pool::request(a, &v)?;
                self.log(&format!("panel: network audio: {did}"));
                Ok(())
            }
            "network-devices/do" => {
                let v: serde_json::Value = serde_json::from_slice(b).map_err(|e| e.to_string())?;
                let did = netaudio::do_request(a, &v)?;
                self.log(&format!("panel: network audio: {did}"));
                Ok(())
            }
            "route" => {
                let fp = s("fingerprint");
                let v: serde_json::Value = serde_json::from_slice(b).map_err(|e| e.to_string())?;
                let receiver = v
                    .get("receiver")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_owned();
                let mut changes = Vec::new();
                for c in v
                    .get("changes")
                    .and_then(|x| x.as_array())
                    .into_iter()
                    .flatten()
                {
                    let num = |k: &str| {
                        c.get(k)
                            .and_then(|x| x.as_u64())
                            .filter(|&n| n < 256)
                            .map(|n| n as u16)
                    };
                    let ch = num("ch").ok_or("each change needs ch, 0 to 255")?;
                    let tx = c
                        .get("tx")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .trim()
                        .to_owned();
                    let to = if tx.is_empty() {
                        None
                    } else {
                        Some((tx, num("tx_ch").ok_or("each route needs tx_ch, 0 to 255")?))
                    };
                    changes.push(idn_open_protocol::node::routing::RouteChange {
                        receiver: receiver.clone(),
                        ch,
                        to,
                    });
                }
                if receiver.is_empty() || changes.is_empty() {
                    return Err("give a receiver and at least one change".into());
                }
                match a.route_node(&fp, &changes, Duration::from_secs(5)) {
                    Ok(reply) if reply.attr("ok") == Some("true") => {
                        self.log(&format!(
                            "panel: {} route change(s) for {receiver} on {}",
                            changes.len(),
                            fp.get(..16).unwrap_or(&fp)
                        ));
                        Ok(())
                    }
                    Ok(reply) => Err(io::Error::other(
                        reply
                            .attr("error")
                            .unwrap_or("the machine refused")
                            .to_owned(),
                    )),
                    Err(e) => Err(e),
                }
            }
            "option" | "unset" => {
                let scope = match s("scope").as_str() {
                    "" | "site" => OptionScope::Site,
                    "device" => OptionScope::Device(s("target")),
                    "output" => OptionScope::Output(s("target")),
                    "input" => OptionScope::Input(s("target")),
                    other => return Err(format!("no scope {other:?}")),
                };
                if cmd == "option" {
                    a.set_option(&scope, &s("key"), &s("value")).map(|_| ())
                } else {
                    a.unset_option(&scope, &s("key")).map(|_| ())
                }
            }
            "password" => {
                let r = set_password(&self.dir, &s("password"));
                if r.is_ok() {
                    self.sessions.end_all();
                    self.log("panel: site password changed; everyone logs in again");
                }
                r
            }
            _ => Err(io::Error::other("unknown request")),
        };
        r.map_err(|e| e.to_string())
    }
}
