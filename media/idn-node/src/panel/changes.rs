//! Changes from the panel: its settings and the routing matrix's routes.

use super::*;

impl State {
    /// `{receiver, changes: [{ch, tx, tx_ch}]}`, applied together; `tx: ""` clears.
    pub(super) fn set_routes(&self, body: &[u8]) -> Result<(), String> {
        use idn_open_protocol::node::routing::RouteChange;
        let v: Value = serde_json::from_slice(body).map_err(|e| format!("bad JSON: {e}"))?;
        let receiver = v
            .get("receiver")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        let mut changes = Vec::new();
        for c in v
            .get("changes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let num = |k: &str| {
                c.get(k)
                    .and_then(Value::as_u64)
                    .filter(|&n| n < 256)
                    .map(|n| n as u16)
            };
            let ch = num("ch").ok_or("each change needs ch, 0 to 255")?;
            let tx = c
                .get("tx")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_owned();
            let to = if tx.is_empty() {
                None
            } else {
                Some((tx, num("tx_ch").ok_or("each route needs tx_ch, 0 to 255")?))
            };
            changes.push(RouteChange {
                receiver: receiver.clone(),
                ch,
                to,
            });
        }
        if receiver.is_empty() || changes.is_empty() {
            return Err("give a receiver and at least one change".into());
        }
        self.core
            .apply_routes(&changes, false)
            .map_err(|e| e.to_string())
    }

    /// `limited`: a session from a token a program asked for while programs need approval
    /// (F6-1): who may use the Node, its LAN password, site and passphrase are refused.
    pub(super) fn settings(&self, body: &[u8], limited: bool) -> Result<(), String> {
        let v: Value = serde_json::from_slice(body).map_err(|e| format!("bad JSON: {e}"))?;
        if limited
            && [
                "approve_programs",
                "program",
                "lan_password",
                "passphrase",
                "server",
                // (The Site Manager's password is the site administrator's.)
                "sitemanager",
            ]
            .iter()
            .any(|k| v.get(k).is_some())
        {
            return Err("this panel was opened by a program: approvals, the LAN password, the site and its Site Manager are changed from the panel opened with the IDN Node shortcut (idn-session --open), or as administrator (sudo idn-node open)".into());
        }
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        // A key that is present changes the setting; "" clears it.
        let opt = |k: &str| {
            v.get(k)
                .map(|x| x.as_str().map(str::to_owned).filter(|x| !x.is_empty()))
        };
        // `"video": {…}` / `"audio": {…}`: each member a setting by name, as text.
        let edits = |k: &str| -> Vec<(String, String)> {
            v.get(k)
                .and_then(Value::as_object)
                .map(|o| {
                    o.iter()
                        .map(|(k, x)| {
                            let x = match x {
                                Value::String(t) => t.clone(),
                                Value::Null => String::new(),
                                Value::Bool(b) => if *b { "on" } else { "off" }.to_owned(),
                                other => other.to_string(),
                            };
                            (k.clone(), x)
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        // The machine's own rate: 44100, 48000, 96000, or "" / null for the site's; anything
        // else refused (bug run J1-6: it cleared the machine's own).
        let audio_rate = match v.get("audio_rate") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(Value::String(t)) if t.trim().is_empty() => Some(None),
            Some(r) => {
                let hz = match r {
                    Value::Number(n) => n
                        .as_u64()
                        .and_then(|n| u32::try_from(n).ok())
                        .filter(|&n| idn_open_protocol::node::rate::valid(n)),
                    Value::String(t) => idn_open_protocol::node::rate::parse(t),
                    _ => None,
                };
                match hz {
                    Some(h) => Some(Some(h)),
                    None => {
                        return Err("audio_rate: 44100, 48000 or 96000 (empty: the site's)".into());
                    }
                }
            }
        };
        self.core
            .set(Change {
                name: s("name"),
                server: opt("server"),
                interface: opt("interface"),
                passphrase: opt("passphrase"),
                require: v.get("require").and_then(Value::as_bool),
                admin_code: s("admin_code").filter(|c| !c.is_empty()),
                lan_password: opt("lan_password"),
                lan_password_hash: None,
                // The panels' simple-mode PIN (§N.20); "" wipes it.
                panel_pin: v
                    .get("panel_pin")
                    .and_then(Value::as_str)
                    .map(|p| Some(p.to_owned()).filter(|p| !p.is_empty())),
                panel_pin_hash: None,
                panel_site_only: None,
                from_site: false,
                site_lock: None,
                services_off: None,
                auto_site: v.get("auto_site").and_then(Value::as_bool),
                welcomed: v.get("welcomed").and_then(Value::as_bool),
                video: edits("video"),
                audio: edits("audio"),
                capture: edits("capture"),
                sitelink: edits("sitelink"),
                aes67: edits("aes67"),
                listen: edits("listen"),
                data: edits("data"),
                omt: edits("omt"),
                captions: edits("captions"),
                netaudio: edits("netaudio"),
                routers: edits("routers"),
                sitemanager: edits("sitemanager"),
                apps: edits("apps"),
                service: None,
                service_now: None,
                service_auto: None,
                keep_awake: v.get("keep_awake").and_then(Value::as_bool),
                // "clock": {"on": bool, "domain": n, "priority": n | null (automatic)}
                clock: v.pointer("/clock/on").and_then(Value::as_bool),
                clock_domain: v
                    .pointer("/clock/domain")
                    .and_then(Value::as_u64)
                    .and_then(|d| u8::try_from(d).ok()),
                clock_protocol: v
                    .pointer("/clock/protocol")
                    .and_then(Value::as_str)
                    .and_then(idn_open_protocol::ptp::Protocol::parse),
                clock_priority: v
                    .pointer("/clock/priority")
                    .map(|p| p.as_u64().and_then(|p| u8::try_from(p).ok())),
                // "audio_rate": 44100 | 48000 | 96000 | "" or null (the site's)
                audio_rate,
                approve_programs: v.get("approve_programs").and_then(Value::as_bool),
                link_requests: v.get("link_requests").and_then(Value::as_bool),
                // "program": {"action": "allow"|"block"|"forget", "path": …, "app": …}
                program: v.get("program").and_then(|p| {
                    let t = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
                    let path = t("path");
                    (!path.is_empty()).then(|| (t("action"), path, t("app")))
                }),
                // (Who updates the machine: root's `idn-node set` or the Site Manager.)
                update_managed_by: None,
            })
            .map_err(|e| e.to_string())
    }
}
