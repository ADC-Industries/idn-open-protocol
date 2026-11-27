//! The panel's requests: who sent them and what they may do (N4, §N.31), each answered,
//! and the changes it sends.

use super::*;

impl State {
    /// A request the shared front leaves to the Site Manager (`crate::front`), for `user`
    /// (a session's, or a host's typed call): a change's role checked
    /// ([`State::may_run`]), then [`respond`](Self::respond); a backup and a restore
    /// written to the audit as theirs (logging in writes its own line; logging out and a
    /// release's pieces aren't changes).
    pub(super) fn answer_as(&self, r: Request, user: Option<String>) -> Response {
        // The role now (a user removed or changed since logging in has it at once).
        let role = user.as_deref().and_then(|u| self.users.role(u));
        let user = user.filter(|_| role.is_some()).unwrap_or_default();
        let change = role.is_some()
            && r.method == "POST"
            && r.path.starts_with("/api/v1/")
            && r.path != "/api/v1/login";
        let audited = change && matches!(r.path.as_str(), "/api/v1/backup" | "/api/v1/restore");
        let cmd = action_of(&r.path).to_owned();
        // (Parsed only where audited: not every POST before the session is looked at.)
        let body: serde_json::Value = if audited {
            serde_json::from_slice(&r.body).unwrap_or_default()
        } else {
            serde_json::Value::Null
        };
        let peer = r.peer.to_string();
        let client: String = r
            .header("x-idn-client")
            .unwrap_or("")
            .chars()
            .filter(|c| !c.is_control())
            .take(64)
            .collect();
        if change && let Err(why) = self.may_run(&user, &cmd, &body) {
            if audited {
                self.users.audit(&user, &peer, &cmd, &body, Some(&why));
            }
            return Response::json_err("403 Forbidden", &why);
        }
        let resp = self.respond(r, &user, role);
        if change {
            self.status_changed();
        }
        if audited {
            self.audit_answer(&user, (&peer, &client), &cmd, &body, &resp);
        }
        resp
    }

    /// Crash reports, logs and alerts (§N.16, §N.41): `GET /api/v1/reports` and the push
    /// channel's `reports`.
    pub(super) fn reports(&self) -> serde_json::Value {
        let a = &self.authority;
        let unix = |t: std::time::SystemTime| {
            t.duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
        };
        let crashes: Vec<serde_json::Value> = a
                .crash_reports()
                .into_iter()
                .map(|(fp, name, size, at)| serde_json::json!({"fingerprint": fp, "app": crash_app(&name), "name": name, "size": size, "at": unix(at)}))
                .collect();
        let logs: Vec<serde_json::Value> = a
                .log_snapshots()
                .into_iter()
                .map(|(fp, snap, files)| serde_json::json!({"fingerprint": fp, "snapshot": snap,
                    "files": files.into_iter().map(|(f, s)| serde_json::json!({"file": f, "size": s})).collect::<Vec<_>>()}))
                .collect();
        // With the typed event each carries (§N.41): its name and fields.
        let alerts: Vec<serde_json::Value> = a
            .alert_entries()
            .into_iter()
            .take(200)
            .map(|x| {
                serde_json::json!({"at": x.at, "fingerprint": x.fingerprint, "app": x.app,
                    "level": x.level, "text": x.text,
                    "event": (!x.event.is_empty()).then_some(x.event),
                    "detail": serde_json::from_str::<serde_json::Value>(&x.detail).ok()})
            })
            .collect();
        serde_json::json!({"crashes": crashes, "logs": logs, "alerts": alerts})
    }

    pub(super) fn respond(&self, r: Request, user: &str, role: Option<users::Role>) -> Response {
        let logged_in = role.is_some();
        // A machine fetching its update's package (§N.23): the token is the key.
        if r.method == "GET"
            && let Some(rest) = r.path.strip_prefix("/release/")
        {
            let (version, platform) = rest.split_once('/').unwrap_or((rest, ""));
            let num = |k: &str| r.param(k).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
            return match self.updates.piece(
                version,
                platform,
                num("offset"),
                num("len") as usize,
                r.param("token").unwrap_or(""),
            ) {
                Ok(b) => Response::ok("application/octet-stream", b),
                Err(status) => Response::text(status, "no"),
            };
        }
        // The site's metrics for a collector (N5): with the token, not a login.
        if r.method == "GET" && r.path == "/metrics" {
            let given = web::bearer(&r).unwrap_or("");
            let want = std::fs::read_to_string(self.dir.join(METRICS_TOKEN)).unwrap_or_default();
            let got = idn_open_protocol::hash::sha256_hex(given.as_bytes());
            // Compared whole, not stopping at the first difference.
            let same =
                idn_open_protocol::trust::proto::same(want.trim().as_bytes(), got.as_bytes());
            return if given.is_empty() || !same {
                Response::text("401 Unauthorized", "a metrics token is needed")
            } else {
                Response::ok(
                    "text/plain; version=0.0.4; charset=utf-8",
                    self.watch.metrics(&self.authority),
                )
            };
        }
        match (r.method.as_str(), r.path.as_str()) {
            // The typed commands' JSON schema (POST /api/v1/<path> bodies).
            ("GET", "/api/v1/schema") => Response::json(crate::typed::schema().to_string()),
            // No UI of its own (the UI is the client's): what this is, for a person
            // or a program that opens the address.
            ("GET", "/") => Response::json(
                serde_json::json!({
                    "service": "IDN Site Manager",
                    "api": "/api/v1/",
                    "events": "/api/v1/events",
                    "password_set": self.password().is_some(),
                    "logged_in": logged_in,
                })
                .to_string(),
            ),
            // The site CA certificate (PEM), what clients pin for the machines' network
            // ports (extensions §E.10.11). Public: compare its fingerprint with
            // `site.ca` in the status.
            ("GET", "/api/v1/site_ca") => Response::ok(
                "application/x-pem-file",
                self.authority.ca_pem().into_bytes(),
            ),
            ("POST", "/api/v1/login") => {
                if !r.may_change("X-IDN-Node") {
                    return Response::text("403 Forbidden", "refused");
                }
                // The check and the count in one step (overlapping tries all
                // slipped under the limit when counted after the slow password check).
                if !self.logins.attempt(r.peer) {
                    return Response::json_err(
                        "429 Too Many Requests",
                        "too many tries; wait a minute",
                    );
                }
                let pw = field(&r.body, "password").unwrap_or_default();
                let name = field(&r.body, "user").unwrap_or_default();
                let peer = r.peer.to_string();
                let who = serde_json::json!({ "user": name.trim() });
                // (Argon2: a few at once, so logins sent together can't take the memory.)
                let found = web::PasswordChecks::shared()
                    .run_blocking(|| self.users.verify(&name, &pw, self.password().as_deref()));
                match found {
                    Some((name, role)) => {
                        self.logins.succeeded(r.peer);
                        let sid = self.sessions.create_for(&name);
                        self.log(&format!(
                            "panel: {name} ({}) logged in from {peer}",
                            role.as_str()
                        ));
                        self.users.audit(&name, &peer, "login", &who, None);
                        ok().with("Set-Cookie", &self.sessions.cookie("idn_site", &sid, true))
                    }
                    None => {
                        self.log(&format!("panel: wrong password from {peer}"));
                        self.users
                            .audit("", &peer, "login", &who, Some("wrong name or password"));
                        Response::json_err("403 Forbidden", "wrong name or password")
                    }
                }
            }
            (_, p) if p.starts_with("/api/") && !logged_in => {
                Response::text("403 Forbidden", "not logged in")
            }
            // The status's JSON schema (refactor 4).
            ("GET", "/api/v1/status_schema") => {
                Response::json(crate::status::status_schema().to_string())
            }
            // Who is logged in in it, so the page offers what they may do (§N.31). (Also
            // the shared front's `GET /api/v1/state`, with `?after=VERSION`.)
            ("GET", "/api/v1/status") => Response::json(self.status_for(user, role).to_string())
                .with("Cache-Control", "no-store"),
            // The matrix's meters (idn-routing.md §A.15): the levels of every machine
            // with transmitters, asked of each at once (0.4 s at most), `{"tx": {name:
            // [dB, …]}}` (a transmitter's name holds its machine's).
            ("GET", "/api/v1/levels") => {
                Response::json(site_levels(&self.authority)).with("Cache-Control", "no-store")
            }
            // The routing matrix with the network's devices (crate::matrix), for any UI.
            ("GET", "/api/v1/matrix") => Response::json(
                crate::matrix::build(&self.authority.node_reports(), &self.ds_sources())
                    .to_string(),
            )
            .with("Cache-Control", "no-store"),
            // Each transmitter's receivers (idn-routing.md §A.16): who receives it, what,
            // and how; the matrix's `received_by`, by itself.
            ("GET", "/api/v1/audio_receivers") => Response::json(
                crate::matrix::receivers_of_outputs(&crate::matrix::build(
                    &self.authority.node_reports(),
                    &self.ds_sources(),
                ))
                .to_string(),
            )
            .with("Cache-Control", "no-store"),
            // Typed events (§N.41): the newest first (`?n=`, 100 by default, 500 at most).
            ("GET", "/api/v1/events/recent") => {
                let n = r
                    .param("n")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(100usize)
                    .min(500);
                Response::json(serde_json::Value::Array(self.watch.events.newest(n)).to_string())
                    .with("Cache-Control", "no-store")
            }
            // A machine's settings form (§N.21): the groups and keys, and a machine's values.
            ("GET", "/api/v1/settings_schema") => {
                Response::json(crate::settings::schema().to_string())
            }
            ("GET", "/api/v1/machine_settings") => {
                let fp = r.param("fingerprint").unwrap_or_default();
                match self
                    .authority
                    .node_reports()
                    .into_iter()
                    .find(|n| n.fingerprint == fp)
                {
                    Some(n) => Response::json(
                        serde_json::Value::Object(crate::settings::values_of(&n.report))
                            .to_string(),
                    )
                    .with("Cache-Control", "no-store"),
                    None => Response::json_err(
                        "404 Not Found",
                        "that machine isn't reporting now (offline, or no IDN Node)",
                    ),
                }
            }
            // The network's Dante-compatible devices as each machine's idn-aoip read them
            // (roadmap R.32 stage 1a, idn-aoip-controller.md K.1): read only.
            ("GET", "/api/v1/network-devices") => {
                Response::json(netaudio::devices_json(&self.authority.node_reports()).to_string())
                    .with("Cache-Control", "no-store")
            }
            // One device's settings as it says them now, its choices and the ops offered
            // for it (R.32 stage 2, idn-aoip-controller.md K.14): asked of a machine's Node.
            ("GET", "/api/v1/network-devices/settings") => {
                let p = |k: &str| r.param(k).unwrap_or_default().to_owned();
                match netaudio::settings_read(&self.authority, &p("device"), &p("machine")) {
                    Ok(v) => Response::json(v.to_string()).with("Cache-Control", "no-store"),
                    Err(e) => Response::json_err("400 Bad Request", &e),
                }
            }
            // The audit (§N.31): everyone logged in reads it.
            ("GET", "/api/v1/audit") => {
                let n = r
                    .param("n")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(200usize)
                    .min(5000);
                Response::json(serde_json::Value::Array(self.users.audit_log(n)).to_string())
            }
            ("GET", "/api/v1/users") if role < Some(users::Role::Admin) => {
                Response::json_err("403 Forbidden", "administrators only")
            }
            ("GET", "/api/v1/users") => Response::json(
                serde_json::Value::Array(
                    self.users
                        .list()
                        .into_iter()
                        .map(|(n, r)| serde_json::json!({"name": n, "role": r.as_str()}))
                        .collect(),
                )
                .to_string(),
            ),
            // Crash reports and logs (idn-node.md §N.16).
            ("GET", "/api/v1/reports") => Response::json(self.reports().to_string()),
            ("GET", "/api/v1/report") | ("GET", "/api/v1/logfile") => {
                let p = |k: &str| r.param(k).unwrap_or_default().to_owned();
                let (text, name) = if r.path == "/api/v1/report" {
                    (
                        self.authority.crash_report(&p("fingerprint"), &p("name")),
                        p("name"),
                    )
                } else {
                    (
                        self.authority
                            .log_file(&p("fingerprint"), &p("snapshot"), &p("file")),
                        format!("{}-{}", p("snapshot"), p("file")),
                    )
                };
                match text {
                    Ok(t) => {
                        let resp = Response::ok("text/plain; charset=utf-8", t);
                        if r.param("download").is_some() {
                            resp.with(
                                "Content-Disposition",
                                &format!(
                                    "attachment; filename=\"{}\"",
                                    name.replace(['"', '\\'], "_")
                                ),
                            )
                        } else {
                            resp
                        }
                    }
                    Err(e) => Response::text("404 Not Found", &e.to_string()),
                }
            }
            ("POST", _) if !r.may_change("X-IDN-Node") => Response::text(
                "403 Forbidden",
                "refused: missing X-IDN-Node header or foreign origin",
            ),
            // A backup of the site (§N.17): the file itself.
            ("POST", "/api/v1/backup") => {
                let pass = field(&r.body, "passphrase").unwrap_or_default();
                match idn_open_protocol::backup::make(&self.dir, "site-manager", BACKUP_SKIP, &pass)
                {
                    Ok(b) => {
                        self.log(&format!("panel: backup downloaded from {}", r.peer));
                        let t = idn_open_protocol::localtime::now();
                        let name = format!(
                            "site-backup-{}-{:04}{:02}{:02}.idnsite",
                            idn_open_protocol::discovery::hostname().to_lowercase(),
                            t.year,
                            t.month,
                            t.day
                        );
                        Response::ok("application/octet-stream", b).with(
                            "Content-Disposition",
                            &format!("attachment; filename=\"{name}\""),
                        )
                    }
                    Err(e) => Response::json_err("400 Bad Request", &e.to_string()),
                }
            }
            // A restore (§N.17): into a Site Manager with no machines yet; it takes effect
            // at the next start.
            // The machine this Site Manager runs on doesn't count: it is given a
            // certificate from the restored keys at the start (F6-4).
            ("POST", "/api/v1/restore") => {
                let host = idn_open_protocol::backup::site_host()
                    .map(|(spki, _)| idn_open_protocol::trust::pki::fingerprint(&spki));
                if self
                    .authority
                    .devices()
                    .iter()
                    .any(|d| Some(&d.fingerprint) != host.as_ref())
                {
                    return Response::json_err(
                        "409 Conflict",
                        "machines have joined this site: restore only into a Site Manager that only its own machine has joined",
                    );
                }
                let pass = field(&r.body, "passphrase").unwrap_or_default();
                let data = field(&r.body, "data").unwrap_or_default();
                let r2 = idn_common::text::unbase64_r(data.trim())
                    .map_err(|_| std::io::Error::other("the file didn't arrive whole"))
                    .and_then(|b| idn_open_protocol::backup::open(&b, &pass))
                    .and_then(|(m, files)| {
                        if m.kind != "site-manager" {
                            return Err(std::io::Error::other(format!(
                                "a backup of {:?}, not of a Site Manager",
                                m.kind
                            )));
                        }
                        idn_open_protocol::backup::stage(&files, &self.dir).map(|()| m)
                    });
                match r2 {
                    Ok(m) => {
                        self.log(&format!(
                            "panel: a backup of {} staged; restart the Site Manager to finish",
                            m.created
                        ));
                        Response::json(serde_json::json!({"ok": true, "created": m.created, "files": m.files.len()}).to_string())
                    }
                    Err(e) => Response::json_err("400 Bad Request", &e.to_string()),
                }
            }
            // Settings to machines, and templates (idn-node.md §N.21).
            // Releases and updates (§N.23).
            ("POST", "/api/v1/release/piece") => {
                let offset = r.param("offset").and_then(|v| v.parse().ok()).unwrap_or(0);
                match self.updates.upload_piece(offset, &r.body) {
                    Ok(()) => ok(),
                    Err(e) => Response::json_err("400 Bad Request", &e.to_string()),
                }
            }
            ("GET", "/api/v1/api_keys") => {
                Response::json(serde_json::Value::Array(self.api_keys()).to_string())
            }
            ("POST", "/api/v1/logout") => {
                self.sessions.end(r.cookie("idn_site"));
                ok()
            }
            _ => Response::text("404 Not Found", "not found"),
        }
    }
}

/// The program a crash report is of, from its name `crash-<app>-<date>-<time>-<pid>.txt`
/// (a program's name has hyphens itself); the name without `crash-` when it isn't so.
fn crash_app(name: &str) -> String {
    let rest = name.strip_prefix("crash-").unwrap_or(name);
    let digits = |s: &str, n: Option<usize>| {
        !s.is_empty() && n.is_none_or(|n| s.len() == n) && s.bytes().all(|b| b.is_ascii_digit())
    };
    if let Some(stem) = rest.strip_suffix(".txt") {
        let parts: Vec<&str> = stem.rsplitn(4, '-').collect();
        if let [pid, time, date, app] = parts[..]
            && digits(pid, None)
            && digits(time, Some(6))
            && digits(date, Some(8))
        {
            return app.to_owned();
        }
    }
    rest.to_owned()
}

/// The matrix's levels from every machine with transmitters (`{"tx": …}`), asked of each
/// at once (0.4 s at most). One answer serves every page for 200 ms (two pages open asked
/// every machine twice as often), and a machine that didn't answer in time is left out
/// for 5 s (its asks piled up, a thread each).
pub(super) fn site_levels(a: &Authority) -> String {
    use std::collections::HashMap;
    use std::time::Instant;
    static LAST: Mutex<Option<(Instant, String)>> = Mutex::new(None);
    static SLOW: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);
    // (Held while asking: a second page's request waits and takes the same answer.)
    let mut last = LAST.lock_ok();
    if let Some((at, body)) = last.as_ref()
        && at.elapsed() < Duration::from_millis(200)
    {
        return body.clone();
    }
    let skip: Vec<String> = {
        let mut slow = SLOW.lock_ok();
        let slow = slow.get_or_insert_with(HashMap::new);
        slow.retain(|_, t| t.elapsed() < Duration::from_secs(5));
        slow.keys().cloned().collect()
    };
    let asks: Vec<_> = a
        .node_reports()
        .into_iter()
        .filter(|r| {
            r.report.attr("levels") == Some("1")
                && r.report.children_named("audio_tx").next().is_some()
        })
        .map(|r| r.fingerprint)
        .filter(|fp| !skip.contains(fp))
        .map(|fp| {
            let a = a.clone();
            // (How long it took measured in its own thread: joined after a slow one, a
            // quick failure was taken for slow too.)
            let h = std::thread::spawn({
                let fp = fp.clone();
                move || {
                    let t0 = Instant::now();
                    let r = a.node_levels(&fp, Duration::from_millis(400));
                    (r, t0.elapsed())
                }
            });
            (fp, h)
        })
        .collect();
    let mut tx = serde_json::Map::new();
    for (fp, h) in asks {
        match h.join() {
            Ok((Ok(l), _)) => {
                if let serde_json::Value::Object(m) =
                    idn_open_protocol::node::meters::levels_json(&l)
                {
                    tx.extend(m);
                }
            }
            Ok((Err(_), took)) if took >= Duration::from_millis(400) => {
                SLOW.lock_ok()
                    .get_or_insert_with(HashMap::new)
                    .insert(fp, Instant::now());
            }
            _ => {}
        }
    }
    let body = serde_json::json!({ "tx": tx }).to_string();
    *last = Some((Instant::now(), body.clone()));
    body
}

/// A request's name in the audit: the command's, as the shared front writes it (`route`,
/// not `/api/v1/route`).
pub(super) fn action_of(path: &str) -> &str {
    path.strip_prefix("/api/v1/").unwrap_or(path)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_crash_reports_program() {
        for (name, app) in [
            ("crash-idn-node-20261117-140501-4242.txt", "idn-node"),
            (
                "crash-idn-site-manager-20261117-140501-1.txt",
                "idn-site-manager",
            ),
            ("crash-odd.txt", "odd.txt"),
            ("crash-x-2026-140501-1.txt", "x-2026-140501-1.txt"),
        ] {
            assert_eq!(super::crash_app(name), app, "{name}");
        }
    }
}
