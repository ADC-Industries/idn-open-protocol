//! The application server's side (the MQTT broker, §I.7): machines as topics, their
//! status and the site's events, what they sent as messages, commands and requests.

use super::*;

impl SiteManager {
    /// A machine's name as a topic level: letters, digits, `-`, `_`, `.`, lower case; the
    /// fingerprint's start added if another machine has it.
    pub(super) fn unique_topic(&self, name: &str, fp: &str) -> String {
        let mut t: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect::<String>()
            .trim_matches('-')
            .chars()
            .take(64)
            .collect();
        if t.is_empty() || t == "site" {
            t = format!("machine-{}", &fp[..8]);
        }
        let taken = self
            .machines()
            .unwrap_or_default()
            .iter()
            .any(|m| m.topic == t && m.fingerprint != fp);
        if taken {
            t = format!("{t}-{}", &fp[..6]);
        }
        t
    }

    pub(super) fn machine_by_topic(&self, topic: &str) -> Option<Machine> {
        self.machines()
            .ok()?
            .into_iter()
            .find(|m| m.topic == topic && !m.revoked)
    }

    /// `idn/<machine>/status`, retained.
    pub(super) fn publish_status(&self, fp: &str) {
        let Some(m) = self
            .machines()
            .ok()
            .and_then(|v| v.into_iter().find(|m| m.fingerprint == fp))
        else {
            return;
        };
        if m.topic.is_empty() {
            return;
        }
        let body = serde_json::json!({
            "online": m.online.is_some(),
            "name": m.name,
            "fingerprint": m.fingerprint,
            "edition": m.edition,
            "version": m.version,
            "tags": m.tags,
            "revoked": m.revoked,
            "transport": m.online.as_ref().map(|o| o.transport.clone()),
            "address": m.online.as_ref().map(|o| o.address.clone()),
            "since_ms": m.online.as_ref().map(|o| o.since_ms),
            "clock_offset_ms": m.online.as_ref().map(|o| o.clock_offset_ms),
            "last_seen_ms": m.last_seen_ms,
        });
        let msg = Msg::new(
            &format!("idn/{}/status", m.topic),
            serde_json::to_vec(&body).unwrap_or_default(),
            1,
            true,
        );
        let _ = self.0.broker.publish(&msg);
    }

    /// Status and `idn/site/events` as things happen.
    pub(super) async fn announce(self) {
        let mut ev = self.events();
        loop {
            let e = match ev.recv().await {
                Ok(e) => e,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            };
            let (what, fp) = match &e {
                Event::Online(fp) => ("online", fp.clone()),
                Event::Offline(fp) => ("offline", fp.clone()),
                Event::Enrolled(m) => ("enrolled", m.fingerprint.clone()),
                Event::Revoked(fp) => ("revoked", fp.clone()),
                Event::Stored(_) => continue,
            };
            self.publish_status(&fp);
            let m = self.machine(&fp);
            let body = serde_json::json!({
                "event": what,
                "machine": m.as_ref().map(|m| m.topic.clone()),
                "name": m.as_ref().map(|m| m.name.clone()),
                "fingerprint": fp,
                "time_ms": wire::now_ms(),
            });
            let _ = self.0.broker.publish(&Msg::new(
                "idn/site/events",
                serde_json::to_vec(&body).unwrap_or_default(),
                1,
                false,
            ));
        }
    }

    /// What a machine sent, as MQTT messages (§I.7.2).
    /// `tags`: the machine's tags when the item arrived (the Site Manager's record, never
    /// the machine's word), as the `tags` property.
    pub(super) fn to_mqtt(m: &Machine, arrived_ms: i64, it: &Item, tags: &str) -> Vec<Msg> {
        let props = |msg: &mut Msg| {
            msg.props.user = vec![
                ("id".into(), it.id_hex()),
                ("time_ms".into(), it.time_ms.to_string()),
                ("arrived_ms".into(), arrived_ms.to_string()),
                ("machine".into(), m.topic.clone()),
                ("fingerprint".into(), m.fingerprint.clone()),
                ("tags".into(), tags.to_owned()),
            ];
        };
        match it.op {
            wire::OP_VALUES => {
                let Ok(vals) = serde_json::from_slice::<Vec<crate::node::Value>>(&it.payload)
                else {
                    return Vec::new();
                };
                vals.into_iter()
                    .filter_map(|v| {
                        let point = point_topic(&v.name)?;
                        let body = serde_json::json!({
                            "value": v.value, "time_ms": v.time_ms, "quality": v.quality,
                        });
                        let mut msg = Msg::new(
                            &format!("idn/{}/v/{point}", m.topic),
                            serde_json::to_vec(&body).unwrap_or_default(),
                            1,
                            true,
                        );
                        props(&mut msg);
                        Some(msg)
                    })
                    .collect()
            }
            // A Node's local MQTT, bridged up (§I.7.4): `idn/<machine>/local/<topic>`.
            wire::OP_MESSAGE if it.kind == "idn.mqtt" => {
                let Some(mut msg) = crate::mqtt::bridge_decode(&it.payload) else {
                    return Vec::new();
                };
                if !rumqttc::v5::mqttbytes::valid_topic(&msg.topic) {
                    return Vec::new();
                }
                msg.topic = format!("idn/{}/local/{}", m.topic, msg.topic);
                msg.qos = 1;
                // The item's id and times added to the message's own user properties.
                let mut tmp = Msg::new("", Vec::new(), 0, false);
                props(&mut tmp);
                msg.props.user.extend(tmp.props.user);
                vec![msg]
            }
            wire::OP_MESSAGE | wire::OP_FILE => {
                let level = if it.op == wire::OP_FILE { "f" } else { "m" };
                let mut msg = Msg::new(
                    &format!("idn/{}/{level}/{}", m.topic, it.kind),
                    it.payload.clone(),
                    1,
                    false,
                );
                props(&mut msg);
                vec![msg]
            }
            _ => Vec::new(),
        }
    }

    /// Hands the inbox to the broker, in order. A message (or file) no persistent
    /// session took stays (the site's hold, 7 days unless set; 0: until taken) and goes to
    /// the first that subscribes; values are retained, so they go at once.
    pub(super) async fn deliver(self) {
        let mut ev = self.events();
        let mut cursor = 0u64;
        // Items at or below this were published to everyone once already.
        let mut published_upto = 0u64;
        // The held items are read again from the start when a session subscribes (one that
        // may take them); and, as a fallback, every [`RESCAN_EVERY`] while a persistent
        // session exists, else every [`RESCAN_IDLE`] (items past the hold let go). The
        // fallback's deadline is kept across passes, so traffic doesn't push it back.
        let mut fallback = Fallback::new(std::time::Instant::now());
        loop {
            if fallback.due(std::time::Instant::now()) {
                // Reached here too, not only while waiting, so a site that never runs
                // out of new items still reads the held ones again.
                if self.fallback_fires(&mut fallback) {
                    cursor = 0;
                }
            }
            let me = self.clone();
            let batch = tokio::task::spawn_blocking(move || me.inbox(cursor, 256))
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default();
            if batch.is_empty() {
                let deadline = tokio::time::Instant::from_std(fallback.next);
                tokio::select! {
                    _ = ev.recv() => {}
                    _ = self.0.rescan.notified() => {
                        cursor = 0;
                        fallback.read_all(std::time::Instant::now());
                    }
                    _ = tokio::time::sleep_until(deadline) => {
                        if self.fallback_fires(&mut fallback) {
                            cursor = 0;
                        }
                    }
                }
                continue;
            }
            // Those delivered (or whose machine is gone) marked done together, in one commit
            // off the runtime's threads, after the batch.
            let mut done = Vec::new();
            for (seq, fp, at, it) in batch {
                cursor = seq;
                let Some(m) = self.machine(&fp) else {
                    done.push(seq);
                    continue;
                };
                let again = seq <= published_upto;
                let mut stored = 0;
                let tags = self.inbox_tags(seq).unwrap_or_else(|| tags_text(&m.tags));
                for msg in Self::to_mqtt(&m, at, &it, &tags) {
                    stored += if again {
                        self.0.broker.publish_persistent(&msg).unwrap_or(0)
                    } else {
                        self.0.broker.publish(&msg).map_or(0, |r| r.0)
                    };
                }
                published_upto = published_upto.max(seq);
                let hold = self.hold_days() as i64;
                let young = hold == 0 || wire::now_ms() - at < hold * 24 * 3600 * 1000;
                let keep = it.op != wire::OP_VALUES && stored == 0 && young;
                if !keep {
                    done.push(seq);
                }
            }
            let me = self.clone();
            let _ = tokio::task::spawn_blocking(move || me.inbox_done_all(&done)).await;
        }
    }

    /// Runs the due fallback: whether the held items are read again from the start now
    /// (the deadline moves on either way). Asking the broker is cheap: it reads no queue.
    fn fallback_fires(&self, fallback: &mut Fallback) -> bool {
        let persistent = self.0.broker.has_persistent_session();
        fallback.fire(std::time::Instant::now(), persistent)
    }

    pub(super) async fn hooks(self, mut hooks: tokio::sync::mpsc::UnboundedReceiver<Hook>) {
        while let Some(h) = hooks.recv().await {
            match h {
                Hook::Subscribed => self.0.rescan.notify_one(),
                Hook::Answer { id, msg, .. } => {
                    let status = user_prop(&msg, "status")
                        .and_then(|s| s.parse::<u8>().ok())
                        .unwrap_or(wire::ST_OK);
                    if let Some(tx) = self.0.answers.lock_ok().remove(&id) {
                        let _ = tx.send((status, msg.payload.to_vec()));
                    }
                }
                Hook::Down {
                    machine,
                    topic,
                    mut msg,
                } => {
                    let Some(m) = self.machine_by_topic(&machine) else {
                        continue;
                    };
                    if !rumqttc::v5::mqttbytes::valid_topic(&topic) {
                        continue;
                    }
                    msg.topic = topic;
                    let it = Item::new(
                        wire::OP_MESSAGE,
                        "idn.mqtt",
                        crate::mqtt::bridge_encode(&msg),
                    );
                    if let Err(e) = self.send_down(&m.fingerprint, &it) {
                        idn_open_protocol::log::log(&format!(
                            "site manager: down to {machine}: {e}"
                        ));
                    }
                }
                // IDN Voice's commands (`idn/site/voice/cmd/…`, §I.18).
                Hook::Published(m) => {
                    let hub = self.0.voice.lock_ok().as_ref().and_then(|w| w.upgrade());
                    if let Some(h) = hub {
                        h.mqtt_command(&m.topic, &m.payload);
                    }
                }
                Hook::Nack { machine, id, msg } => {
                    let body: serde_json::Value = serde_json::from_slice(&msg.payload)
                        .unwrap_or_else(|_| {
                            serde_json::json!({ "reason": String::from_utf8_lossy(&msg.payload) })
                        });
                    let me = self.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        if let Err(e) = me.refuse(&id, &body, &format!("mqtt ({machine})")) {
                            idn_open_protocol::log::log(&format!(
                                "site manager: a refusal of {id}: {e}"
                            ));
                        }
                    })
                    .await;
                }
                // (A Node's local broker's; not this one's.)
                Hook::Local { .. } => {}
                Hook::Command {
                    machine,
                    kind,
                    msg,
                    saved,
                } => {
                    tokio::spawn(self.clone().run_command(machine, kind, msg, saved));
                }
            }
        }
    }

    /// A command an application published to `idn/<machine>/cmd/<type>`: held on the
    /// volume (§I.7.3) and sent to the machine (waiting for it to connect until the
    /// message expires, 5 minutes by default), its answer published to the command's
    /// response topic. The command's item id is fixed when it arrives, so a machine that
    /// gets it twice (a Site Manager restart) runs it once.
    pub(super) async fn run_command(
        self,
        machine: String,
        kind: String,
        msg: Msg,
        saved: crate::mqtt::Saved,
    ) {
        let deadline = msg.props.expires_ms.unwrap_or(wire::now_ms() + 300_000);
        let m = match self.machine_by_topic(&machine) {
            None => {
                return self.answer_command(
                    &machine,
                    &msg.props.response_topic,
                    &msg.props.correlation,
                    (
                        wire::ST_UNKNOWN,
                        format!("no machine \"{machine}\"").into_bytes(),
                    ),
                );
            }
            Some(_) if !wire::valid_kind(&kind) || wire::reserved_kind(&kind) => {
                return self.answer_command(
                    &machine,
                    &msg.props.response_topic,
                    &msg.props.correlation,
                    (wire::ST_REFUSED, b"bad type".to_vec()),
                );
            }
            Some(m) => m,
        };
        let held = Held {
            machine,
            fingerprint: m.fingerprint,
            item: b64(store::encode(&Item::new(
                wire::OP_REQUEST,
                &kind,
                msg.payload.to_vec(),
            ))),
            response_topic: msg.props.response_topic.clone(),
            correlation: msg.props.correlation.as_deref().map(b64),
            deadline_ms: deadline,
        };
        let me = self.clone();
        let h = held.clone();
        // Not held (the volume full or failing): answered so, not left without an answer
        // (deep bug run 2026-11-09).
        let r = tokio::task::spawn_blocking(move || me.hold_command(&h)).await;
        // The publisher's PUBACK goes once the command is held (on the volume).
        saved.done(matches!(r, Ok(Ok(()))));
        match r {
            Ok(Ok(())) => self.run_held(held).await,
            Ok(Err(e)) => self.answer_command(
                &held.machine,
                &held.response_topic,
                &msg.props.correlation,
                (
                    wire::ST_ERROR,
                    format!("the command couldn't be held: {e}").into_bytes(),
                ),
            ),
            Err(e) => self.answer_command(
                &held.machine,
                &held.response_topic,
                &msg.props.correlation,
                (
                    wire::ST_ERROR,
                    format!("the command couldn't be held: {e}").into_bytes(),
                ),
            ),
        }
    }

    /// Commands held when the Site Manager stopped, run again.
    pub(super) fn resume_commands(&self) {
        let held: Vec<Held> = self
            .0
            .db
            .begin_read()
            .ok()
            .and_then(|r| r.open_table(COMMANDS).ok())
            .map(|t| {
                t.iter()
                    .map(|i| {
                        i.filter_map(|e| e.ok())
                            .filter_map(|(_, v)| serde_json::from_slice(v.value()).ok())
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        if !held.is_empty() {
            idn_open_protocol::log::log(&format!(
                "site manager: {} command(s) held from before, sent again",
                held.len()
            ));
        }
        for h in held {
            tokio::spawn(self.clone().run_held(h));
        }
    }

    pub(super) fn hold_command_pub(&self, h: &Held) -> io::Result<()> {
        self.hold_command(h)
    }

    pub(super) async fn run_held_pub(self, h: Held) {
        self.run_held(h).await
    }

    fn hold_command(&self, h: &Held) -> io::Result<()> {
        let id = h.id();
        let w = self.0.db.begin_write().map_err(db_err)?;
        w.open_table(COMMANDS)
            .map_err(db_err)?
            .insert(
                id.as_str(),
                serde_json::to_vec(h).unwrap_or_default().as_slice(),
            )
            .map_err(db_err)?;
        w.commit().map_err(db_err)
    }

    fn release_command(&self, id: &str) -> io::Result<()> {
        let w = self.0.db.begin_write().map_err(db_err)?;
        w.open_table(COMMANDS)
            .map_err(db_err)?
            .remove(id)
            .map_err(db_err)?;
        w.commit().map_err(db_err)
    }

    fn command_held(&self, id: &str) -> bool {
        let r = self.0.db.begin_read().ok();
        let t = r.as_ref().and_then(|r| r.open_table(COMMANDS).ok());
        t.is_some_and(|t| t.get(id).ok().flatten().is_some())
    }

    /// Cancels a held command (an administrator, §I.7.3): it isn't sent any more; one
    /// already being run isn't stopped. False when no such command is held.
    pub fn cancel_command(&self, id: &str, by: &str) -> io::Result<bool> {
        let held = self.held_all().into_iter().find(|h| h.id() == id);
        let Some(h) = held else { return Ok(false) };
        self.release_command(id)?;
        let kind = h.item().map(|i| i.kind).unwrap_or_default();
        self.audit(
            by,
            &h.fingerprint,
            "command.cancel",
            serde_json::json!({ "id": id, "type": kind }),
        );
        Ok(true)
    }

    /// Cancels the update offers and rollbacks held for machine `fp` (it was pinned, or
    /// is given another): one held from before mustn't arrive after it.
    pub(super) fn cancel_updates_for(&self, fp: &str, by: &str) {
        for h in self.held_all() {
            let kind = h.item().map(|i| i.kind).unwrap_or_default();
            if h.fingerprint == fp && kind.starts_with("idn.update.") {
                let _ = self.cancel_command(&h.id(), by);
            }
        }
    }

    fn held_all(&self) -> Vec<Held> {
        let r = self.0.db.begin_read().ok();
        let t = r.as_ref().and_then(|r| r.open_table(COMMANDS).ok());
        let Some(t) = t else { return Vec::new() };
        let Ok(i) = t.iter() else { return Vec::new() };
        i.filter_map(|e| e.ok())
            .filter_map(|(_, v)| serde_json::from_slice::<Held>(v.value()).ok())
            .collect()
    }

    /// Commands held now (for the web UI and the API).
    pub fn held_commands(&self) -> Vec<serde_json::Value> {
        let r = self.0.db.begin_read().ok();
        let t = r.as_ref().and_then(|r| r.open_table(COMMANDS).ok());
        let Some(t) = t else { return Vec::new() };
        let Ok(i) = t.iter() else { return Vec::new() };
        i.filter_map(|e| e.ok())
            .filter_map(|(_, v)| serde_json::from_slice::<Held>(v.value()).ok())
            .map(|h| {
                let kind = h.item().map(|i| i.kind).unwrap_or_default();
                serde_json::json!({
                    "id": h.id(), "machine": h.machine, "type": kind,
                    "deadline_ms": h.deadline_ms,
                })
            })
            .collect()
    }

    async fn run_held(self, h: Held) {
        let id = h.id();
        let Some(it) = h.item() else {
            let _ = self.release_command(&id);
            return;
        };
        let deadline = h.deadline_ms;
        let (status, payload) = loop {
            let left = deadline - wire::now_ms();
            if left <= 0 {
                break (
                    wire::ST_TIMEOUT,
                    b"the machine wasn't reachable in time".to_vec(),
                );
            }
            // Cancelled meanwhile (an administrator; a pin, a newer offer): not sent.
            if !self.command_held(&id) {
                return;
            }
            let online = self.0.online.lock_ok().contains_key(&h.fingerprint);
            if !online {
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
            let t = Duration::from_millis(left.min(60_000) as u64);
            match self.command_item(&h.fingerprint, &it, t).await {
                Ok(a) => break a,
                // The link dropped meanwhile, or the program is still at it: the same
                // command again (the machine's claim gives the first run's answer,
                // waiting for it while it runs), until the deadline.
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::NotConnected | io::ErrorKind::TimedOut
                    ) =>
                {
                    continue;
                }
                Err(e) => break (wire::ST_ERROR, e.to_string().into_bytes()),
            }
        };
        let correlation = h.correlation.as_deref().and_then(unb64);
        // The Site Manager's own (an update): its answer in the audit.
        if h.response_topic.is_none() && it.kind.starts_with("idn.") {
            self.audit(
                "site manager",
                &h.fingerprint,
                &format!("{}.answer", it.kind),
                serde_json::json!({
                    "status": status,
                    "answer": String::from_utf8_lossy(&payload),
                }),
            );
        }
        self.answer_command(
            &h.machine,
            &h.response_topic,
            &correlation,
            (status, payload),
        );
        let me = self.clone();
        let _ = tokio::task::spawn_blocking(move || me.release_command(&id)).await;
    }

    fn answer_command(
        &self,
        machine: &str,
        response_topic: &Option<String>,
        correlation: &Option<Vec<u8>>,
        (status, payload): (u8, Vec<u8>),
    ) {
        let Some(rt) = response_topic.clone() else {
            return;
        };
        let mut answer = Msg::new(&rt, payload, 1, false);
        answer.props.correlation = correlation.clone();
        let tags = self
            .machine_by_topic(machine)
            .map(|m| tags_text(&m.tags))
            .unwrap_or_default();
        answer.props.user = vec![
            ("status".into(), status.to_string()),
            ("machine".into(), machine.to_owned()),
            ("tags".into(), tags),
        ];
        let _ = self.0.broker.publish(&answer);
    }

    /// A machine's request: published to `idn/<machine>/req/<type>` with the response
    /// topic `idn/<machine>/res/<id>`; the application's answer goes back.
    pub(super) async fn machine_request(&self, fp: &str, it: Item) -> (u8, Vec<u8>) {
        let Some(m) = self.machine(fp) else {
            return (wire::ST_UNKNOWN, Vec::new());
        };
        let id = it.id_hex();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.0.answers.lock_ok().insert(id.clone(), tx);
        let mut msg = Msg::new(
            &format!("idn/{}/req/{}", m.topic, it.kind),
            it.payload.clone(),
            1,
            false,
        );
        msg.props.response_topic = Some(format!("idn/{}/res/{id}", m.topic));
        msg.props.correlation = Some(it.id.to_vec());
        msg.props.user = vec![
            ("id".into(), id.clone()),
            ("machine".into(), m.topic.clone()),
            ("time_ms".into(), it.time_ms.to_string()),
            ("tags".into(), tags_text(&m.tags)),
        ];
        msg.props.expires_ms = Some(wire::now_ms() + 60_000);
        let reached = self.0.broker.publish(&msg).map(|(a, b)| a + b).unwrap_or(0);
        if reached == 0 {
            self.0.answers.lock_ok().remove(&id);
            return (
                wire::ST_UNKNOWN,
                b"no application subscribed to requests".to_vec(),
            );
        }
        match tokio::time::timeout(Duration::from_secs(60), rx).await {
            Ok(Ok(a)) => a,
            _ => {
                self.0.answers.lock_ok().remove(&id);
                (
                    wire::ST_TIMEOUT,
                    b"the application didn't answer in time".to_vec(),
                )
            }
        }
    }
}

/// A command held on the volume until it is answered or expires (§I.7.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Held {
    /// The machine's topic name, as the application gave it.
    machine: String,
    fingerprint: String,
    /// The command's item (store::encode, base64): its id fixed once.
    item: String,
    response_topic: Option<String>,
    /// Base64.
    correlation: Option<String>,
    deadline_ms: i64,
}

impl Held {
    /// A command of the Site Manager's own for `m` (no response topic).
    pub(super) fn own(m: &Machine, it: &Item, deadline_ms: i64) -> Held {
        Held {
            machine: m.topic.clone(),
            fingerprint: m.fingerprint.clone(),
            item: b64(store::encode(it)),
            response_topic: None,
            correlation: None,
            deadline_ms,
        }
    }

    fn item(&self) -> Option<Item> {
        unb64(&self.item).and_then(|b| store::decode(&b))
    }

    fn id(&self) -> String {
        self.item().map(|i| i.id_hex()).unwrap_or_default()
    }
}

/// The held items' fallback reading while a persistent session exists (a session
/// subscribing reads them at once).
const RESCAN_EVERY: Duration = Duration::from_secs(300);
/// With no persistent session (nothing would take what is held), the fallback reading
/// only for letting go of items past the hold.
const RESCAN_IDLE: Duration = Duration::from_secs(3600);

/// Whether the fallback reads the held items again, [`RESCAN_EVERY`] after the last
/// fallback: with a persistent session, yes; with none, once [`RESCAN_IDLE`] passed since
/// the last reading.
fn fallback_rescan(persistent: bool, since_last: Duration) -> bool {
    persistent || since_last >= RESCAN_IDLE
}

/// The inbox fallback's timing: when it next comes due and when the held items were last
/// read from the start. Kept outside the deliver loop, so new items arriving don't restart
/// the wait.
struct Fallback {
    next: std::time::Instant,
    read: std::time::Instant,
}

impl Fallback {
    fn new(now: std::time::Instant) -> Self {
        Self {
            next: now + RESCAN_EVERY,
            read: now,
        }
    }

    fn due(&self, now: std::time::Instant) -> bool {
        now >= self.next
    }

    /// The held items are read from the start now (a session subscribed, or the fallback).
    fn read_all(&mut self, now: std::time::Instant) {
        self.read = now;
        self.next = now + RESCAN_EVERY;
    }

    /// The fallback came due: whether to read from the start. The next one is
    /// [`RESCAN_EVERY`] on either way.
    fn fire(&mut self, now: std::time::Instant, persistent: bool) -> bool {
        if fallback_rescan(persistent, now.saturating_duration_since(self.read)) {
            self.read_all(now);
            true
        } else {
            self.next = now + RESCAN_EVERY;
            false
        }
    }
}

#[cfg(test)]
mod rescan_tests {
    use super::*;

    /// With no persistent session the held items aren't read again at each fallback
    /// wait, only once the idle period passed; with one, they are.
    #[test]
    fn the_inbox_isnt_read_again_at_each_wait_with_no_session() {
        assert!(!fallback_rescan(false, RESCAN_EVERY));
        assert!(!fallback_rescan(
            false,
            RESCAN_IDLE - Duration::from_secs(1)
        ));
        assert!(fallback_rescan(false, RESCAN_IDLE));
        assert!(fallback_rescan(true, RESCAN_EVERY));
    }

    /// New items every second (each a pass of the deliver loop) don't hold the fallback
    /// off: with a persistent session it fires every [`RESCAN_EVERY`], with none (the
    /// idle reading that lets expired items go) once [`RESCAN_IDLE`] passed.
    #[test]
    fn the_fallback_fires_however_busy_the_site_is() {
        for (persistent, limit) in [(true, RESCAN_EVERY), (false, RESCAN_IDLE)] {
            let t0 = std::time::Instant::now();
            let mut fb = Fallback::new(t0);
            let mut fired = None;
            for s in 1..=2 * RESCAN_IDLE.as_secs() {
                let now = t0 + Duration::from_secs(s);
                if fb.due(now) && fb.fire(now, persistent) {
                    fired = Some(now - t0);
                    break;
                }
            }
            assert_eq!(fired, Some(limit), "persistent: {persistent}");
        }
    }
}
