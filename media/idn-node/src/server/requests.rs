//! The Node's control requests (from its panel, programs, the Site Manager), one by one.

use super::*;

/// Relayed pairings' tickets kept for the programs that asked (as many as the
/// membership keeps answers for).
pub(super) const KEY_TICKETS: usize = 64;

impl NodeCore {
    /// The program on connection `cid` ("" when not known).
    /// Whether program `cid` may relay pairings with the site's code: the program that
    /// hosts this Node (the same process, as the system tells it) or an
    /// administrator. Any other program here gets no code, and its relayed requests wait
    /// for a person (bug run 2026-11-25: on a relay machine any program read the code and
    /// verified its own request). By process, not by executable path: paths compared after
    /// `canonicalize` never matched on Windows (`\\?\`), nor on Linux after an upgrade
    /// replaced the running file (bug run B).
    fn may_relay(&self, cid: u64) -> bool {
        self.clients.lock_ok().get(&cid).is_some_and(|c| {
            // (The executable known means the system gave the process too, not the
            // program's own word in its hello.)
            c.admin || (!c.exe.is_empty() && c.pid == std::process::id())
        })
    }

    fn exe_of(&self, cid: u64) -> String {
        self.clients
            .lock_ok()
            .get(&cid)
            .map(|c| c.exe.clone())
            .unwrap_or_default()
    }

    /// One request (§N.4.2) to its reply.
    pub(super) fn handle(&self, cid: u64, m: &Element) -> Element {
        let id = m.attr("id").unwrap_or("0").to_owned();
        let error = |code: &str, text: &str| {
            Element::new("error")
                .with_attr("id", &id)
                .with_attr("code", code)
                .with_attr("text", text)
        };
        let ok = || Element::new("ok").with_attr("id", &id);
        // What uses the machine's identity or changes it waits for the operator (§N.8.1).
        if matches!(
            m.name.as_str(),
            "sign"
                | "admin"
                | "set"
                | "route"
                | "routes"
                | "endpoint"
                | "directory"
                | "panel_token"
                | "services_state"
                | "audio_apps"
                | "displays"
                | "encoders"
                | "log"
                | "media_send"
                // (A pending program could have the Node read any file and pass it on,
                // clear the simple-mode PIN, or try PINs.)
                | "log_file"
                // (Reports written and passed on to the site, the real ones pushed out.)
                | "crash"
                | "panel_pin"
                | "panel_unlock"
                | "sas"
                | "data_panel"
                | "pair_answer"
                | "pair_confirm"
                | "site_confirm"
                | "site_join"
                | "pair_remove"
                | "pair_request"
                // (Another program's receivers slowed by up to half a second; a one-time
                // link kept from lapsing.)
                | "receiver_latency"
                | "receiver_floor"
                | "pair_use"
                // (Every program's levels, and their metering kept on by asking.)
                | "get_levels"
                // (A site key asked for, a pairing code read, another's key taken, a
                // key's use made up.)
                | "key_request"
                | "pairing_code"
                | "key_answer"
                | "key_used"
                | "key_withdraw"
                // (The links' codes, incoming ones too.)
                | "pairs"
        ) {
            let exe = self
                .clients
                .lock_ok()
                .get(&cid)
                .map(|c| c.exe.clone())
                .unwrap_or_default();
            // Linking and joining through the Node's own tools (allowed without asking:
            // any program may run them) are for an administrator while programs need
            // approval: a waiting program read a link's code with `idn-node link`, then
            // had `idn-node link accept CODE` answer it (bug run 4).
            let own_tool = installed_with_node(&exe)
                && !self
                    .settings()
                    .programs
                    .iter()
                    .any(|r| super::same_path(&r.path, &exe));
            if own_tool
                && self.settings().approve_programs
                && matches!(
                    m.name.as_str(),
                    "pairs"
                        | "pair_answer"
                        | "pair_confirm"
                        | "pair_request"
                        | "pair_remove"
                        | "pair_use"
                        | "site_join"
                )
                && !self.clients.lock_ok().get(&cid).is_some_and(|c| c.admin)
            {
                return error(
                    "denied",
                    "while programs need approval, linking and joining from the command line are for root / an elevated administrator (or use the panel)",
                );
            }
            // A panel session likewise (bug run 2026-11-25: a waiting program ran
            // `idn-node open`, read the address it printed, and linked the machine, cleared
            // the PIN or switched services through the panel): only the session agent's
            // `--open` (it opens the browser, printing nothing) and an administrator.
            let session_agent = std::path::Path::new(&exe)
                .file_stem()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.eq_ignore_ascii_case("idn-session"));
            if own_tool
                && self.settings().approve_programs
                && m.name == "panel_token"
                && !session_agent
                && !self.clients.lock_ok().get(&cid).is_some_and(|c| c.admin)
            {
                return error(
                    "denied",
                    "while programs need approval, the panel is opened with the IDN Node shortcut (idn-session --open) or as root / an elevated administrator",
                );
            }
            match self.approval(&exe) {
                Approval::Allowed => {}
                Approval::Pending => {
                    return error(
                        "approval",
                        "this program waits for approval in the IDN Node's panel (Programs)",
                    );
                }
                Approval::Blocked => {
                    return error(
                        "approval",
                        "this program is blocked in the IDN Node's panel (Programs)",
                    );
                }
            }
        }
        match m.name.as_str() {
            // The media folder, the files to and from other machines, and the site's
            // machines a file can go to (§N.25).
            "media" => {
                let mut e = self.media.element().with_attr("id", &id);
                if let Some(c) = &self.member.read_ok().channel {
                    for d in c.devices() {
                        e = e.with_child(
                            Element::new("machine")
                                .with_attr("fp", &d.fingerprint)
                                .with_attr("name", &d.name),
                        );
                    }
                }
                e
            }
            // Sends a file of the media folder to a machine of the site (by fingerprint
            // or name), whose Node keeps it in its own media folder.
            "media_send" => {
                let dir = self.media.dir();
                let path = match (m.attr("path"), m.attr("file")) {
                    (Some(p), _) => PathBuf::from(p),
                    (None, Some(f)) => dir.join(f),
                    _ => return error("bad", "path or file: a file of the media folder"),
                };
                if !crate::media::inside(&dir, &path) {
                    return error(
                        "bad",
                        &format!(
                            "{}: not a file in the media folder {} (only those can be sent)",
                            path.display(),
                            dir.display()
                        ),
                    );
                }
                let Some(ch) = self.member.read_ok().channel.clone() else {
                    return error("no_site", "this machine isn't in a site");
                };
                let to = m.attr("to").unwrap_or("");
                let Some(peer) = ch
                    .devices()
                    .into_iter()
                    .find(|d| d.fingerprint == to || d.name == to)
                else {
                    return error("bad", &format!("no machine {to:?} in the site now"));
                };
                let file = path
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let size = std::fs::metadata(&path).map_or(0, |x| x.len());
                // `replace`: the box replaces a finished file of the name (once all of the
                // new one is there and checked) instead of keeping "NAME (2).EXT" beside it
                // (2026-11-26).
                let replace = m.attr("replace") == Some("true");
                let offer = format!(
                    "{}{}-{file}",
                    if replace {
                        crate::media::REPLACE_PREFIX
                    } else {
                        crate::media::ID_PREFIX
                    },
                    self.next.fetch_add(1, Ordering::Relaxed)
                );
                // Noted before it is offered: the box may fetch it all before the answer
                // is back here ("sent", §N.25).
                self.media.note(crate::media::Transfer {
                    way: "out",
                    id: offer.clone(),
                    peer: peer.name.clone(),
                    peer_fp: peer.fingerprint.clone(),
                    file,
                    size,
                    done: Arc::new(AtomicU64::new(0)),
                    state: "offered".into(),
                    at: idn_common::epoch::secs(),
                });
                // Within a program's 5 s for an answer: accepting is at once, the file
                // follows (the receiving Node fetches it).
                let r = ch.push(&peer.fingerprint, &offer, &path, Duration::from_secs(4));
                let state = match &r {
                    Ok(idn_open_protocol::channel::Reply::Ok(_)) => "offered".to_owned(),
                    Ok(idn_open_protocol::channel::Reply::Refused) => "refused".to_owned(),
                    Ok(x) => format!("failed: {x:?}"),
                    Err(e) => format!("failed: {e}"),
                };
                if state == "offered" {
                    ok().with_attr("to", &peer.name).with_attr("offer", &offer)
                } else {
                    self.media
                        .set_out_state(&offer, &peer.fingerprint, state.clone());
                    error("refused", &format!("{}: {state}", peer.name))
                }
            }
            "identity" => self.identity().with_attr("id", &id),
            "clock" => self.clock_element().with_attr("id", &id),
            "sign" => {
                let Some(data) = m.attr("data").and_then(unb64) else {
                    return error("bad", "no data");
                };
                match self.sign(m.attr("key").unwrap_or(""), &data) {
                    Ok(sig) => Element::new("signed")
                        .with_attr("id", &id)
                        .with_attr("sig", &b64(&sig)),
                    Err((code, text)) => {
                        if code == "denied" {
                            self.log("refused to sign something that isn't a TLS handshake");
                        }
                        error(code, &text)
                    }
                }
            }
            "options" => {
                let o = self.enrollment().map(|e| e.options()).unwrap_or_default();
                o.to_element().with_attr("id", &id)
            }
            // The site's audio transmitters and their channel counts (§E.12.12), as the
            // site last said (asked again when older than 5 s): a desk's routing view.
            // A named API key used by a program here (idn-node.md §N.42): to the site.
            // Ask the site for its newest release for this machine now (§N.42).
            "update_check" => match self.check_updates() {
                Ok(()) => ok(),
                Err(e) => error("state", &e),
            },
            // A client's pairing relayed by a program here (a host program on a machine
            // without the Site Manager, idn-node.md §N.42.3): to the site, and its answer
            // back.
            "key_request" => match self.enrollment() {
                Some(e) => match e.request_key(
                    m.attr("client").unwrap_or(""),
                    m.attr("pin")
                        .filter(|p| !p.is_empty() && self.may_relay(cid)),
                ) {
                    Ok(t) => {
                        let mut kt = self.key_tickets.lock_ok();
                        kt.insert(t, (self.exe_of(cid), cid));
                        while kt.len() > KEY_TICKETS {
                            kt.pop_first();
                        }
                        ok().with_attr("ticket", &t.to_string())
                    }
                    Err(x) => error("failed", &x.to_string())
                        .with_attr("kind", &format!("{:?}", x.kind())),
                },
                None => error("state", "not in a site"),
            },
            "pairing_code" if !self.may_relay(cid) => error(
                "denied",
                "the pairing code is for the program hosting this IDN Node (or an administrator)",
            ),
            "pairing_code" => match self.enrollment() {
                Some(e) => match e.pairing_code_left(Duration::from_secs(5)) {
                    Ok((code, until, left)) => ok()
                        .with_attr("code", &code)
                        .with_attr("until", &until.to_string())
                        .with_attr("left", &left.as_secs().to_string()),
                    Err(x) => error("failed", &x.to_string())
                        .with_attr("kind", &format!("{:?}", x.kind())),
                },
                None => error("state", "not in a site"),
            },
            "key_answer" => match (
                self.enrollment(),
                m.attr("ticket").and_then(|t| t.parse::<u64>().ok()),
            ) {
                // (Only the program that asked is answered: an answer is taken once, and
                // it is a site key.)
                (Some(_), Some(t))
                    if !self.key_tickets.lock_ok().get(&t).is_some_and(|(exe, by)| {
                        // (By its program file; one the system didn't say: by its
                        // connection.)
                        if exe.is_empty() {
                            *by == cid
                        } else {
                            *exe == self.exe_of(cid)
                        }
                    }) =>
                {
                    error("bad", "no such pairing")
                }
                (Some(e), Some(t)) => match e.key_status(t) {
                    idn_open_protocol::trust::KeyStatus::Waiting { code_wrong } => {
                        let w = ok().with_attr("waiting", "true");
                        if code_wrong {
                            w.with_attr("code", "wrong")
                        } else {
                            w
                        }
                    }
                    idn_open_protocol::trust::KeyStatus::Answered(g) => ok()
                        .with_attr("approved", if g.approved { "true" } else { "false" })
                        .with_attr("key_id", &g.id)
                        .with_attr("key", &g.key)
                        .with_attr("scope", &g.scope)
                        .with_attr("engine", &g.engine)
                        .with_attr("error", &g.error),
                },
                (None, _) => error("state", "not in a site"),
                _ => error("bad", "no ticket"),
            },
            // A relayed request taken back by the program that asked (§N.42.3).
            "key_withdraw" => match (
                self.enrollment(),
                m.attr("ticket").and_then(|t| t.parse::<u64>().ok()),
            ) {
                (Some(e), Some(t)) => {
                    let mine = self.key_tickets.lock_ok().get(&t).is_some_and(|(exe, by)| {
                        if exe.is_empty() {
                            *by == cid
                        } else {
                            *exe == self.exe_of(cid)
                        }
                    });
                    if !mine {
                        error("bad", "no such pairing")
                    } else {
                        // (Kept while its answer is still to collect: a key that came
                        // just before is the program's; bug run 2026-11-24.)
                        match e.withdraw_key_request(t) {
                            Ok(()) => {
                                self.key_tickets.lock_ok().remove(&t);
                                ok()
                            }
                            Err(x) => error("failed", &x.to_string())
                                .with_attr("kind", &format!("{:?}", x.kind())),
                        }
                    }
                }
                (None, _) => error("state", "not in a site"),
                _ => error("bad", "no ticket"),
            },
            "key_used" => {
                if let Some(e) = self.enrollment() {
                    e.send_to_authority(
                        &Element::new("idn_key_used").with_attr("key", m.attr("key").unwrap_or("")),
                    );
                }
                ok()
            }
            "site_transmitters" => match self.enrollment() {
                Some(e) => e.site_transmitters().into_iter().fold(
                    Element::new("site_transmitters").with_attr("id", &id),
                    |out, t| {
                        let mut tx = Element::new("tx")
                            .with_attr("name", &t.name)
                            .with_attr("channels", &t.channels.to_string())
                            .with_attr("machine", &t.machine);
                        // Where its sound comes from (§A.16), where the program said.
                        if let Some(o) = &t.origin {
                            for (k, v) in o.attrs() {
                                tx = tx.with_attr(k, &v);
                            }
                        }
                        out.with_child(tx)
                    },
                ),
                None => Element::new("site_transmitters").with_attr("id", &id),
            },
            "directory" => match self.enrollment() {
                Some(e) => {
                    e.watch_directory();
                    directory_element(&e, &id)
                }
                None => error("state", "not in a site"),
            },
            // One file-channel endpoint a machine: the Node's own while it runs (§N.25).
            "endpoint" if self.member.read_ok().channel.is_some() => {
                self.log("a program's file channel: refused (the Node's is this machine's)");
                error(
                    "in_use",
                    "the IDN Node's file channel is this machine's endpoint (idn-node.md §N.25)",
                )
            }
            "endpoint" => match (
                self.enrollment(),
                m.attr("port").and_then(|p| p.parse().ok()),
            ) {
                (Some(e), Some(port)) => {
                    e.announce_endpoint(port);
                    ok()
                }
                (None, _) => error("state", "not in a site"),
                _ => error("bad", "no port"),
            },
            "admin" => {
                let (Some(e), Some(req)) = (self.enrollment(), m.children.first()) else {
                    return error("state", "not in a site, or no request");
                };
                match e.admin(req, Duration::from_secs(10)) {
                    Ok(r) => r.with_attr("id", &id),
                    Err(x) => error("failed", &x.to_string())
                        .with_attr("kind", &format!("{:?}", x.kind())),
                }
            }
            // A program's log file and its crash reports (§N.16).
            "log_file" => {
                if let Some(path) = m.attr("path").filter(|p| !p.is_empty()) {
                    // Its account: a number on Unix, SIDs on Windows.
                    let owner = self
                        .clients
                        .lock_ok()
                        .get(&cid)
                        .and_then(|c| c.owner.clone());
                    self.remember_log_file(m.attr("app").unwrap_or(""), path, owner);
                }
                ok()
            }
            // The panels' simple mode (§N.20): is there a PIN, is it right, a new one.
            "panel_lock" => {
                let s = self.settings();
                panel_element(&s, self.site_locked()).with_attr("id", &id)
            }
            // A one-time token for IDN Data's panel, for `idn data --open` when IDN Data
            // already runs; with `key`, the key itself, for an IDN Data this Node didn't
            // start (so the Node's tokens open it). An approved program
            // only (above), which could ask for tokens anyway.
            "data_panel" if m.attr("key") == Some("1") => {
                ok().with_attr("key", &self.data_panel_key())
            }
            "data_panel" => ok().with_attr("t", &self.data_panel_token()),
            // Ctrl+Alt+Del for a screen under remote control (R.25): Windows lets only a
            // service raise it, and the screen's capture runs in the user's session, so it
            // asks here (an approved program only, above).
            "sas" if !self.service.load(Ordering::Relaxed) => error(
                "unsupported",
                "Ctrl+Alt+Del needs the IDN Node running as the service",
            ),
            "sas" => match idn_input::send_sas() {
                Ok(()) => ok(),
                Err(e) => error("failed", &e.to_string()),
            },
            "panel_unlock" => match self.check_panel_pin(m.attr("pin").unwrap_or("")) {
                Ok(()) => ok(),
                Err(e) => error("denied", &e),
            },
            "panel_pin" => match self.set(Change {
                panel_pin: Some(m.attr("pin").map(str::to_owned).filter(|p| !p.is_empty())),
                ..Default::default()
            }) {
                Ok(()) => ok(),
                Err(e) => error("refused", &e.to_string()),
            },
            // A program's alert (§N.18): logged, and on to the Site Manager.
            "alert" => {
                let (app, level) = (m.attr("app").unwrap_or(""), m.attr("level").unwrap_or(""));
                let text: String = m.text.chars().take(1000).collect();
                self.log(&format!("{app} {level}: {text}"));
                // A typed event (§N.41): on this Node's push channel, and to the Site
                // Manager once a minute at most per event and subject.
                let event = m.attr("event").filter(|e| !e.is_empty() && e.len() <= 64);
                if let Some(ev) = event {
                    let mut fields: serde_json::Value = m
                        .attr("detail")
                        .filter(|d| d.len() <= 2048)
                        .and_then(|d| serde_json::from_str(d).ok())
                        .filter(serde_json::Value::is_object)
                        .unwrap_or_else(|| serde_json::json!({}));
                    fields["app"] = serde_json::json!(app);
                    self.problem_events.emit(ev, level, &text, fields);
                    return ok();
                }
                super::queue_alert(
                    &self.alerts,
                    super::Alert {
                        at: idn_common::epoch::secs(),
                        app: app.to_owned(),
                        level: level.to_owned(),
                        text,
                        ..Default::default()
                    },
                );
                ok()
            }
            "crash" => match self.keep_crash(
                m.attr("name").unwrap_or(""),
                m.attr("app").unwrap_or(""),
                &m.text,
            ) {
                Ok(()) => ok(),
                Err(e) => error("bad", &e.to_string()),
            },
            "register" => {
                let kind = m.attr("kind").unwrap_or("output").to_owned();
                let name = m.attr("name").unwrap_or("").to_owned();
                // Within bounds: each goes into the report to the Site Manager, which a
                // program could otherwise push past a message's size.
                if kind.len() > 32
                    || name.len() > 256
                    || m.attr("handle").is_some_and(|h| h.len() > 64)
                    || m.attr("labels").is_some_and(|l| l.len() > 16 * 1024)
                {
                    return error("bad", "a kind, name, handle or labels too long");
                }
                // The program's own handle (sent again after a reconnect), else a new one.
                let handle = m
                    .attr("handle")
                    .filter(|h| !h.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| self.next.fetch_add(1, Ordering::Relaxed).to_string());
                let channels_attr = if kind == "audio_rx" {
                    "channels"
                } else {
                    "audio_channels"
                };
                let channels = m
                    .attr(channels_attr)
                    .and_then(|c| c.parse::<u16>().ok())
                    .filter(|c| (1..=idn_open_protocol::node::routing::MAX_CHANNELS).contains(c));
                if kind == "audio_rx" {
                    if name.trim().is_empty() || channels.is_none() {
                        return error("bad", "a receiver needs a name and 1 to 256 channels");
                    }
                    // One program per receiver name on the machine.
                    let taken = self.clients.lock_ok().iter().find_map(|(c, cl)| {
                        cl.regs
                            .iter()
                            .any(|(h, r)| {
                                r.kind == "audio_rx"
                                    && r.name == name
                                    && (*c != cid || *h != handle)
                            })
                            .then(|| cl.app.clone())
                    });
                    if let Some(app) = taken {
                        return error("in_use", &format!("receiver {name:?} is offered by {app}"));
                    }
                }
                let reg = Reg {
                    kind: kind.clone(),
                    name: name.clone(),
                    port: m.attr("port").and_then(|p| p.parse().ok()),
                    channels,
                    labels: parse_labels(m.attr("labels")),
                    states: BTreeMap::new(),
                    need_ms: None,
                    signal: Default::default(),
                    levels: None,
                    status: m
                        .attr("status")
                        .filter(|s| s.len() <= 16 * 1024)
                        .map(str::to_owned),
                    // What a receiver is, and an output's sound's origin (§A.16).
                    rx_kind: m
                        .attr("rx_kind")
                        .map(|k| idn_open_protocol::node::routing::ReceiverKind::parse(k).name())
                        .filter(|k| *k != "other")
                        .unwrap_or("")
                        .to_owned(),
                    origin: idn_open_protocol::node::routing::AudioOrigin::from_element(m).filter(
                        |o| {
                            o.kind.len() <= 32
                                && o.device.len() <= 256
                                && m.attr("origin_channels").map_or(0, str::len) <= 16 * 1024
                        },
                    ),
                    via: BTreeMap::new(),
                };
                {
                    // What registrations carry in all (name, labels, status), per program
                    // and for the machine: each goes into the Node's status and its report
                    // to the Site Manager, which a few large ones pushed past a message's
                    // size, the Node's status failing for every program and the machine
                    // falling off its site (deep bug run 2026-11-09).
                    const PER_PROGRAM: usize = 64 * 1024;
                    const PER_MACHINE: usize = 512 * 1024;
                    let size = |r: &Reg| {
                        r.name.len()
                            + r.labels.iter().map(|l| l.len()).sum::<usize>()
                            + r.status.as_ref().map_or(0, String::len)
                            + r.origin.as_ref().map_or(0, |o| {
                                o.device.len() + o.channels.iter().map(String::len).sum::<usize>()
                            })
                    };
                    let mut clients = self.clients.lock_ok();
                    let (mut mine, mut all) = (size(&reg), size(&reg));
                    // (A program's share counted over all its connections: one that opened
                    // eight filled the machine's, and no other program could register; bug
                    // review 3.)
                    // (A program file the system didn't say is no program's: its own
                    // connection only.)
                    let exe = clients
                        .get(&cid)
                        .map(|c| c.exe.clone())
                        .filter(|e| !e.is_empty());
                    for (c, cl) in clients.iter() {
                        let same_program = *c == cid || Some(&cl.exe) == exe.as_ref();
                        for (h, r) in &cl.regs {
                            if *c == cid && *h == handle {
                                continue;
                            }
                            all += size(r);
                            if same_program {
                                mine += size(r);
                            }
                        }
                    }
                    if mine > PER_PROGRAM || all > PER_MACHINE {
                        return error("bad", "too much registered (names, labels and status)");
                    }
                    if let Some(c) = clients.get_mut(&cid) {
                        if c.regs.len() >= 256 && !c.regs.contains_key(&handle) {
                            return error("bad", "this program has 256 registrations already");
                        }
                        c.regs.insert(handle.clone(), reg);
                    }
                }
                let mut reply = Element::new("registered")
                    .with_attr("id", &id)
                    .with_attr("handle", &handle);
                if kind == "audio_rx" {
                    let routes = self.routes_for(&name);
                    // Also as an event: a registration sent again after a reconnect isn't
                    // waited for, and the program still needs its routes.
                    self.tell(cid, &routes);
                    reply = reply.with_child(routes);
                }
                reply
            }
            "route_state" => {
                let rx = m.attr("receiver").unwrap_or("");
                let states: BTreeMap<u16, String> = m
                    .children_named("ch")
                    .filter_map(|c| Some((c.attr("n")?.parse().ok()?, c.attr("state")?.to_owned())))
                    .collect();
                let need = m.attr("need_ms").and_then(|v| v.parse::<u32>().ok());
                // The channels with a signal arriving (§A.15).
                let signal: std::collections::BTreeSet<u16> = m
                    .children_named("ch")
                    .filter(|c| c.attr("sig") == Some("1"))
                    .filter_map(|c| c.attr("n")?.parse().ok())
                    .collect();
                // The transport each channel's sound comes by (§A.16).
                let via: BTreeMap<u16, String> = m
                    .children_named("ch")
                    .filter_map(|c| {
                        let v = c.attr("via").filter(|v| !v.is_empty() && v.len() <= 16)?;
                        Some((c.attr("n")?.parse().ok()?, v.to_owned()))
                    })
                    .collect();
                if let Some(c) = self.clients.lock_ok().get_mut(&cid) {
                    for r in c.regs.values_mut() {
                        if r.kind == "audio_rx" && r.name == rx {
                            r.states = states.clone();
                            r.need_ms = need;
                            r.signal = signal.clone();
                            r.via = via.clone();
                        }
                    }
                }
                ok()
            }
            // An output's levels while a matrix watches (§A.15): kept for the matrix.
            "levels" => {
                let db = m.attr("db").unwrap_or("");
                if let (Some(h), true) = (m.attr("handle"), db.len() <= 8 * 256) {
                    let db: String = idn_open_protocol::node::meters::parse(db)
                        .iter()
                        .map(i32::to_string)
                        .collect::<Vec<_>>()
                        .join(",");
                    if let Some(r) = self
                        .clients
                        .lock_ok()
                        .get_mut(&cid)
                        .and_then(|c| c.regs.get_mut(h))
                        .filter(|r| r.kind == "output")
                    {
                        r.levels = Some((Instant::now(), db));
                    }
                }
                ok()
            }
            // This machine's transmitters' levels, for a matrix (the IDN Mixer's panel).
            "get_levels" => self.levels_element().with_attr("id", &id),
            // A route change from this machine (`idn-node route`, idn-routing.md §A.3).
            "route" => match RouteChange::parse(m).and_then(|c| self.apply_routes(&[c], false)) {
                Ok(()) => ok(),
                Err(e) => error("bad", &e.to_string()),
            },
            // Several at once, all or none (§A.4: a matrix's drag run from a program, as
            // the Site Manager's batches are; bug round 2026-11-09).
            "routes" => match m
                .children_named("route")
                .map(RouteChange::parse)
                .collect::<io::Result<Vec<_>>>()
                .and_then(|all| self.apply_routes(&all, false))
            {
                Ok(()) => ok(),
                Err(e) => error("bad", &e.to_string()),
            },
            // A receiver's sync-group floor from this machine (§A.14; the Site Manager's
            // come in `idn_node_set`): refused on a machine the site locked.
            "receiver_floor" => {
                if self.site_locked() {
                    error("locked", "this machine is locked by its site")
                } else {
                    self.set_receiver_floor(
                        m.attr("receiver").unwrap_or(""),
                        m.attr("ms").and_then(|v| v.parse().ok()),
                    );
                    ok()
                }
            }
            // A receiver's latency from this machine (idn-routing.md §A.12).
            "receiver_latency" => match receiver_latency(m).and_then(|ms| {
                self.set_receiver_latency(m.attr("receiver").unwrap_or(""), ms, false)
            }) {
                Ok(()) => ok(),
                Err(e) => error("bad", &e.to_string()),
            },
            "unregister" => {
                if let (Some(h), Some(c)) = (m.attr("handle"), self.clients.lock_ok().get_mut(&cid))
                {
                    c.regs.remove(h);
                }
                ok()
            }
            "status" => self.status().with_attr("id", &id),
            // Linked computers (§N.32).
            "pairs" => self.pairs_status().with_attr("id", &id),
            "pair_request" => {
                let addr = m
                    .attr("addr")
                    .zip(m.attr("port"))
                    .and_then(|(a, p)| format!("{a}:{p}").parse().ok());
                match addr {
                    Some(a) => match self.pair_ask(a) {
                        Ok((name, code)) => ok().with_attr("name", &name).with_attr("code", &code),
                        Err(e) => error("bad", &e),
                    },
                    None => error("bad", "addr and port: the computer to ask"),
                }
            }
            "pair_confirm" => match self.pair_confirm(m.attr("ok") == Some("true"), m.attr("code"))
            {
                Ok(()) => ok(),
                Err(e) => error("bad", &e),
            },
            "pair_answer" => match self.pair_answer(
                m.attr("accept") == Some("true"),
                m.attr("remember") == Some("true"),
                m.attr("code"),
            ) {
                Ok(()) => ok(),
                Err(e) => error("bad", &e),
            },
            // A program's sessions using a link (a one-time link ends with the last).
            "pair_use" => {
                if let (Some(fp), Some(n)) =
                    (m.attr("fp"), m.attr("n").and_then(|v| v.parse().ok()))
                {
                    self.pairs.used(cid, fp, n);
                }
                ok()
            }
            "pair_find" => self
                .pair_find()
                .iter()
                .fold(Element::new("machines").with_attr("id", &id), |e, f| {
                    e.with_child(crate::pairs::found_element(f))
                }),
            // This machine's person: the Site Manager shows the same code, or not (§N.11).
            "site_confirm" => match self.enrollment() {
                Some(e) => {
                    e.confirm_code(m.attr("matches") == Some("true"));
                    ok()
                }
                None => error("bad", "no site being joined"),
            },
            // A join code from the site's Site Manager, typed here (§N.11.8).
            "site_join" => match self.join_code(m.attr("code").unwrap_or("")) {
                Ok(()) => ok(),
                Err(e) => error("bad", &e),
            },
            // Leaves the site (§N.42); as changing the site's address: while programs need
            // approval, only root or an elevated administrator.
            "site_leave" => {
                if self.settings().approve_programs
                    && !self.clients.lock_ok().get(&cid).is_some_and(|c| c.admin)
                {
                    return error(
                        "denied",
                        "leaving the site is done from the panel, or as root / an elevated administrator",
                    );
                }
                match self.leave_site() {
                    Ok(()) => ok(),
                    Err(e) => error("bad", &e),
                }
            }
            "pair_remove" => match self.pair_remove(m.attr("fp").unwrap_or("")) {
                Ok(()) => ok(),
                Err(e) => error("bad", &e),
            },
            // The programs with sound open in the session runner's session (for the
            // panel's "Programs' sound"): at most 200, names cut to 100 characters.
            "audio_apps" => {
                let cut = |v: Option<&str>| v.unwrap_or("").chars().take(100).collect::<String>();
                let list = m
                    .children_named("app")
                    .take(200)
                    .map(|x| {
                        (
                            cut(x.attr("exe")),
                            cut(x.attr("name")),
                            x.attr("playing") == Some("true"),
                        )
                    })
                    .filter(|(exe, ..)| !exe.is_empty())
                    .collect();
                self.set_audio_apps(list);
                ok()
            }
            // The session runner's displays (for "Local Video Sources"): at most 32, each
            // attribute cut to 300 characters.
            "displays" => {
                let keep = [
                    "id",
                    "name",
                    "width",
                    "height",
                    "hz",
                    "primary",
                    "position",
                    "connector",
                ];
                let list = m
                    .children_named("display")
                    .take(32)
                    .filter(|d| d.attr("id").is_some_and(|i| !i.is_empty()))
                    .map(|d| {
                        let mut e = Element::new("display");
                        for k in keep {
                            if let Some(v) = d.attr(k) {
                                e = e.with_attr(k, &v.chars().take(300).collect::<String>());
                            }
                        }
                        e
                    })
                    .collect();
                self.set_displays(list);
                ok()
            }
            // A line for the Node's log from a program (the session runner's bridges: what a
            // capture or a sound bridge says when it stops), its name before it; at most
            // 300 characters, one line.
            "log" => {
                let who = self
                    .clients
                    .lock_ok()
                    .get(&cid)
                    .map(|c| c.app.clone())
                    .unwrap_or_default();
                let text: String = m
                    .attr("text")
                    .unwrap_or("")
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(300)
                    .collect();
                if !text.is_empty() {
                    let who: String = who.chars().filter(|c| !c.is_control()).take(60).collect();
                    self.log(&format!("{who}: {text}"));
                }
                ok()
            }
            // The encoders that work in the session (the Video page's formats): at most 32.
            "encoders" => {
                let list = m
                    .children_named("encoder")
                    .take(32)
                    .filter_map(|d| {
                        let p = d
                            .attr("processor")
                            .filter(|p| matches!(*p, "gpu" | "cpu"))?;
                        let c = d
                            .attr("codec")
                            .filter(|c| matches!(*c, "h264" | "hevc" | "av1"))?;
                        let by: String = d.attr("by").unwrap_or("").chars().take(40).collect();
                        Some(
                            Element::new("encoder")
                                .with_attr("processor", p)
                                .with_attr("codec", c)
                                .with_attr("by", &by),
                        )
                    })
                    .collect();
                self.set_encoders(list);
                ok()
            }
            // How the services this program runs for the session are doing (§N.14.4).
            "services_state" => {
                let states = m
                    .children_named("service")
                    .take(256)
                    .filter_map(|x| {
                        let st = BridgeState {
                            up: x.attr("up") == Some("true"),
                            restarts: x.attr("restarts").and_then(|v| v.parse().ok()).unwrap_or(0),
                            for_s: x.attr("for_s").and_then(|v| v.parse().ok()).unwrap_or(0),
                            line: x.attr("line").unwrap_or("").chars().take(300).collect(),
                            gave_up: x.attr("gave_up") == Some("true"),
                        };
                        Some((x.attr("id")?.to_owned(), st))
                    })
                    .collect();
                if let Some(c) = self.clients.lock_ok().get_mut(&cid) {
                    c.services = Some(states);
                }
                ok()
            }
            "set" => {
                // Who may use the Node (§N.8.1) is changed from the panel, or by root or an
                // elevated administrator: any program may run `idn-node set`, allowed as
                // the Node's own tool, so a pending one approved itself with it (deep bug
                // run 2026-11-09).
                // (While programs need approval, also the LAN password, the site and its
                // passphrase: with the LAN password set, a pending program logged in to the
                // LAN panel and turned approval off; F6-1.)
                // (And the site's Site Manager: its password is the site administrator's;
                // bug review 3.)
                let approvals = m.attr("approve_programs").is_some()
                    || ["allow", "block", "forget"]
                        .iter()
                        .any(|a| m.attr(&format!("program_{a}")).is_some())
                    || m.attrs.iter().any(|(k, _)| k.starts_with("sitemanager."))
                    || (self.settings().approve_programs
                        && ["lan_password", "passphrase", "server"]
                            .iter()
                            .any(|k| m.attr(k).is_some()));
                if approvals && !self.clients.lock_ok().get(&cid).is_some_and(|c| c.admin) {
                    return error(
                        "denied",
                        "which programs may use the Node is set in the panel, or as root / an elevated administrator",
                    );
                }
                // Who updates this machine (§N.23): root / an elevated administrator (or
                // the Site Manager), as the other product's package would be.
                if m.attr("update.managed_by").is_some()
                    && !self.clients.lock_ok().get(&cid).is_some_and(|c| c.admin)
                {
                    return error(
                        "denied",
                        "who updates this machine is set as root / an elevated administrator, or from the Site Manager",
                    );
                }
                if let Some(v) = m.attr("clock_protocol")
                    && idn_open_protocol::ptp::Protocol::parse(v).is_none()
                {
                    return error("bad_value", "clock_protocol: v2, v1 or both");
                }
                if let Some(v) = m.attr("audio_rate")
                    && !v.is_empty()
                    && idn_open_protocol::node::rate::parse(v).is_none()
                {
                    return error(
                        "bad_value",
                        "audio_rate: 44100, 48000 or 96000 (empty: the site's)",
                    );
                }
                let opt = |k: &str| {
                    m.attr(k)
                        .map(|v| Some(v.to_owned()).filter(|v| !v.is_empty()))
                };
                let r = self.set(Change {
                    name: m.attr("name").map(str::to_owned),
                    server: opt("server"),
                    interface: opt("interface"),
                    passphrase: opt("passphrase"),
                    require: m
                        .attr("require")
                        .map(|v| idn_common::text::parse_on_off(v) == Some(true)),
                    admin_code: m
                        .attr("admin_code")
                        .filter(|c| !c.is_empty())
                        .map(str::to_owned),
                    lan_password: opt("lan_password"),
                    lan_password_hash: None,
                    panel_pin: m.attr("panel_pin").map(|v| Some(v.to_owned())),
                    panel_pin_hash: None,
                    panel_site_only: None,
                    from_site: false,
                    site_lock: None,
                    services_off: None,
                    auto_site: m
                        .attr("auto_site")
                        .map(|v| !idn_common::switches::says_off(v)),
                    welcomed: m
                        .attr("welcomed")
                        .map(|v| !idn_common::switches::says_off(v)),
                    video: video_edits(m),
                    audio: audio_edits(m),
                    capture: capture_edits(m),
                    sitelink: sitelink_edits(m),
                    aes67: aes67_edits(m),
                    listen: listen_edits(m),
                    data: data_edits(m),
                    omt: omt_edits(m),
                    captions: captions_edits(m),
                    netaudio: netaudio_edits(m),
                    routers: routers_edits(m),
                    sitemanager: sitemanager_edits(m),
                    apps: apps_edits(m),
                    service: service_switch(m),
                    service_now: service_now_switch(m),
                    service_auto: None,
                    keep_awake: None,
                    clock: m.attr("clock").map(|v| !idn_common::switches::says_off(v)),
                    clock_domain: m.attr("clock_domain").and_then(|v| v.parse().ok()),
                    clock_protocol: m
                        .attr("clock_protocol")
                        .and_then(idn_open_protocol::ptp::Protocol::parse),
                    clock_priority: m.attr("clock_priority").map(|v| v.parse().ok()),
                    audio_rate: m
                        .attr("audio_rate")
                        .map(idn_open_protocol::node::rate::parse),
                    approve_programs: m
                        .attr("approve_programs")
                        .map(|v| idn_common::text::parse_on_off(v) == Some(true)),
                    link_requests: m
                        .attr("link_requests")
                        .map(|v| idn_common::text::parse_on_off(v) == Some(true)),
                    program: ["allow", "block", "forget"].iter().find_map(|a| {
                        let path = m.attr(&format!("program_{a}"))?;
                        Some((a.to_string(), path.to_owned(), self.app_of(path)))
                    }),
                    update_managed_by: m.attr("update.managed_by").map(str::to_owned),
                });
                match r {
                    Ok(()) => ok(),
                    Err(e) => error("bad", &e.to_string()),
                }
            }
            "panel_token" => {
                let port = self.settings().panel_port;
                // A full session (approvals and the like) for root / an elevated
                // administrator and the session agent's `--open` (installed with the Node;
                // it opens the panel in the browser and prints nothing),
                // or while programs need no approval; else one that can't change them
                // (F6-1). (Not `idn-node open`: it prints the address, and a program
                // waiting for approval ran it, read the token and approved itself; bug
                // review 3.)
                let full = !self.settings().approve_programs
                    || self.clients.lock_ok().get(&cid).is_some_and(|c| {
                        c.admin
                            || (installed_with_node(&c.exe)
                                && std::path::Path::new(&c.exe)
                                    .file_stem()
                                    .and_then(|n| n.to_str())
                                    .is_some_and(|n| n.eq_ignore_ascii_case("idn-session")))
                    });
                Element::new("panel_token").with_attr("id", &id).with_attr(
                    "url",
                    &format!("http://127.0.0.1:{port}/?t={}", self.issue_token(full)),
                )
            }
            "retry" => {
                self.retry();
                ok()
            }
            other => error("unknown", &format!("no request {other:?}")),
        }
    }
}
