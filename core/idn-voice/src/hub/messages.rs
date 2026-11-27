//! Messages and the record: text messages, their history, missed transmissions and replays.

use super::*;

impl Hub {
    // ---- messages

    pub fn message(&self, id: Sid, to: &Target, text: &str) -> Result<(), String> {
        let text = text.trim();
        if text.is_empty() || text.chars().count() > 2000 {
            return Err("a message: 1 to 2,000 characters".into());
        }
        let mut st = self.st.lock_ok();
        {
            let Some(me) = st.sessions.get_mut(&id) else {
                return Err("gone".into());
            };
            while me
                .sent
                .front()
                .is_some_and(|t| t.elapsed() > Duration::from_secs(60))
            {
                me.sent.pop_front();
            }
            if me.sent.len() >= 30 && me.who.feed.is_none() {
                return Err("too many messages: wait a minute".into());
            }
            me.sent.push_back(Instant::now());
        }
        let st = st;
        let Some(me) = st.sessions.get(&id) else {
            return Err("gone".into());
        };
        let from = me.who.name.clone();
        let key = me.who.key().to_owned();
        let handset = me.who.handset.as_ref().map(|h| h.name.clone());
        let (conv, recipients): (String, Vec<Sid>) = match to {
            Target::Channel(c) => {
                if !me.channels.contains_key(c) {
                    return Err("not in that channel".into());
                }
                (
                    store::in_channel(*c),
                    st.sessions
                        .iter()
                        .filter(|(_, s)| s.channels.contains_key(c))
                        .map(|(i, _)| *i)
                        .collect(),
                )
            }
            Target::User(name) => {
                if !me.who.user {
                    return Err("guests can't message a person".into());
                }
                // Between two keys: a driver named as a site user never reads theirs.
                (
                    store::direct(&key, name),
                    st.sessions
                        .iter()
                        .filter(|(_, s)| s.who.answers_to(name) || s.who.answers_to(&key))
                        .map(|(i, _)| *i)
                        .collect(),
                )
            }
            _ => return Err("a channel or a person".into()),
        };
        let m = self
            .store
            .add_message_by(
                &conv,
                &from,
                me.who.ident.as_deref(),
                handset.as_deref(),
                text,
                now_ms(),
            )
            .map_err(|e| e.to_string())?;
        let v = json!({"t": "message", "message": m});
        for r in recipients {
            send(&st.sessions[&r], v.clone());
        }
        if let Target::Channel(c) = to
            && let Some(ch) = st.channels.get(c)
        {
            self.host
                .publish(&format!("messages/{}", ch.name), &json!(m), false);
        }
        Ok(())
    }

    /// A program's message (a dispatch system's HTTP API, cues): into a channel, as `from`.
    pub fn post(&self, channel: u32, from: &str, text: &str) -> Result<store::Message, String> {
        let text = text.trim();
        if text.is_empty() || text.chars().count() > 2000 {
            return Err("a message: 1 to 2,000 characters".into());
        }
        let st = self.st.lock_ok();
        let Some(ch) = st.channels.get(&channel) else {
            return Err("no such channel".into());
        };
        let m = self
            .store
            .add_message(&store::in_channel(channel), from, text, now_ms())
            .map_err(|e| e.to_string())?;
        for s in st
            .sessions
            .values()
            .filter(|s| s.channels.contains_key(&channel))
        {
            send(s, json!({"t": "message", "message": m}));
        }
        self.host
            .publish(&format!("messages/{}", ch.name), &json!(m), false);
        Ok(m)
    }

    /// A command published on the site's MQTT (`idn/site/voice/cmd/…`, §V.10): `message`
    /// `{"channel": name or number, "text", "from"}` into a channel (a cue, a dispatch system);
    /// `driver` `{"handset": id or name, "driver": {…} or null}` and `sos_ack` `{"id",
    /// "by"}` (§V.13), each answered on `idn/site/voice/events`.
    pub fn mqtt_command(&self, topic: &str, payload: &[u8]) {
        let Some(cmd) = topic.strip_prefix("idn/site/voice/cmd/") else {
            return;
        };
        let v: Value = serde_json::from_slice(payload).unwrap_or_default();
        let r = match cmd {
            "message" => {
                let chans = self.channels();
                let c = match &v["channel"] {
                    Value::Number(n) => chans.iter().find(|c| Some(c.id as u64) == n.as_u64()),
                    Value::String(s) => chans.iter().find(|c| c.name.eq_ignore_ascii_case(s)),
                    _ => None,
                };
                let from = v["from"]
                    .as_str()
                    .filter(|f| crate::model::valid_name(f))
                    .unwrap_or("mqtt");
                match c {
                    Some(c) => self
                        .post(c.id, from, v["text"].as_str().unwrap_or_default())
                        .map(|_| ()),
                    None => Err(format!("no channel {}", v["channel"])),
                }
            }
            "driver" => {
                let driver = match &v["driver"] {
                    Value::Null => Ok(None),
                    d => serde_json::from_value::<crate::model::Driver>(d.clone())
                        .map(Some)
                        .map_err(|e| format!("driver: {e}")),
                };
                let handset = v["handset"].as_str().unwrap_or_default();
                let r = driver.and_then(|d| self.assign_driver(handset, d, "mqtt"));
                let mut ev = json!({"event": "driver_result", "handset": handset, "ok": r.is_ok(), "time_ms": now_ms()});
                match &r {
                    Ok(x) => ev["result"] = x.clone(),
                    Err(e) => ev["error"] = json!(e),
                }
                self.host.publish("events", &ev, false);
                r.map(|_| ())
            }
            "sos_ack" => {
                let by = v["by"]
                    .as_str()
                    .filter(|f| crate::model::valid_name(f))
                    .unwrap_or("mqtt");
                match v["id"].as_u64() {
                    Some(id) => self.sos_ack(id, by, None),
                    None => Err("sos_ack: an id".into()),
                }
            }
            other => Err(format!("no command {other}")),
        };
        if let Err(e) = r {
            idn_open_protocol::log::log(&format!("voice: MQTT {topic}: {e}"));
        }
    }

    pub fn history(&self, id: Sid, to: &Target) -> Result<Vec<store::Message>, String> {
        let st = self.st.lock_ok();
        let Some(me) = st.sessions.get(&id) else {
            return Err("gone".into());
        };
        let conv = match to {
            Target::Channel(c) if me.channels.contains_key(c) => store::in_channel(*c),
            Target::User(n) if me.who.user => store::direct(me.who.key(), n),
            _ => return Err("not yours".into()),
        };
        drop(st);
        // (The newest 100: 500 long ones made a reply of megabytes, and a client asking
        // again and again without reading held them all; E5-2.)
        self.store.history(&conv, 100).map_err(|e| e.to_string())
    }

    // ---- recordings

    /// Whether `id` may hear transmission `t`: its channel's member, or one it went to or
    /// came from; administrators any.
    pub fn may_hear(&self, id: Sid, t: &Transmission) -> bool {
        let st = self.st.lock_ok();
        let Some(s) = st.sessions.get(&id) else {
            return false;
        };
        let key = s.who.key();
        s.who.admin
            || t.channel.is_some_and(|c| s.channels.contains_key(&c))
            || (s.who.user && (t.from_key() == key || t.to.iter().any(|x| x == key)))
    }

    /// Transmissions since `since_ms` that `id` may hear, not its own.
    pub fn missed(&self, id: Sid, since_ms: i64) -> Vec<Transmission> {
        let (chans, name, user) = {
            let st = self.st.lock_ok();
            let Some(s) = st.sessions.get(&id) else {
                return Vec::new();
            };
            (
                s.channels.keys().copied().collect::<BTreeSet<u32>>(),
                s.who.key().to_owned(),
                s.who.user,
            )
        };
        self.store
            .transmissions_since(since_ms, 200, |t| {
                t.from_key() != name
                    && (t.channel.is_some_and(|c| chans.contains(&c))
                        || (user && t.to.contains(&name)))
            })
            .unwrap_or_default()
    }

    /// Sends a recording to `id` as replay frames, paced in real time, on a thread.
    pub fn replay(self: &Arc<Self>, id: Sid, tid: u64) -> Result<(), String> {
        let t = self
            .store
            .transmission(tid)
            .map_err(|e| e.to_string())?
            .ok_or("no such recording")?;
        if !self.may_hear(id, &t) {
            return Err("not yours to hear".into());
        }
        if !self.st.lock_ok().replaying.insert(id) {
            return Err("a recording is playing already".into());
        }
        let frames = match crate::record::read(&self.dir, tid) {
            Ok(f) => f,
            Err(e) => {
                self.st.lock_ok().replaying.remove(&id);
                return Err(e.to_string());
            }
        };
        // (Gone meanwhile: its mark taken off too; a session given its number again was
        // refused every replay; bug review 3.)
        let Some(tx) = self.st.lock_ok().sessions.get(&id).map(|s| s.tx.clone()) else {
            self.st.lock_ok().replaying.remove(&id);
            return Err("gone".into());
        };
        if let Some(w) = self.who(id) {
            self.host
                .audit(&w.name, "voice.replay", json!({"id": tid, "from": t.from}));
        }
        let channel = t.channel.unwrap_or(0);
        let me = self.clone();
        tokio::spawn(async move {
            struct Done(Arc<Hub>, Sid);
            impl Drop for Done {
                fn drop(&mut self) {
                    self.0.st.lock_ok().replaying.remove(&self.1);
                }
            }
            let _done = Done(me, id);
            let start = tokio::time::Instant::now();
            for (k, (ms, data)) in frames.iter().enumerate() {
                tokio::time::sleep_until(start + Duration::from_millis(*ms as u64)).await;
                let f = frame(2, 0, channel, k as u32, data);
                if tx.send(Out::Frame(f)).is_err() {
                    return;
                }
            }
            let _ = tx.send(Out::Json(json!({"t": "replay_end", "id": tid})));
        });
        Ok(())
    }
}
