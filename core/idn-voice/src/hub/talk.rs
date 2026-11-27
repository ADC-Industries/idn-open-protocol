//! Talking: who talks where (the floor, priority, limits), and forwarding each talker's sound.

use super::*;

impl Hub {
    pub(super) fn talkers(st: &State, channel: u32) -> Vec<Value> {
        st.sessions
            .iter()
            .filter(|(_, s)| {
                s.talk.as_ref().is_some_and(|t| match t.target {
                    Target::Channel(c) => c == channel,
                    Target::All => s.channels.get(&channel) == Some(&true),
                    _ => false,
                })
            })
            .map(|(id, s)| json!({"sid": id, "name": s.who.name}))
            .collect()
    }

    fn talkers_changed(st: &State, channels: &BTreeSet<u32>) {
        for &c in channels {
            let t = Self::talkers(st, c);
            for s in st.sessions.values().filter(|s| s.channels.contains_key(&c)) {
                send(s, json!({"t": "talkers", "channel": c, "talkers": t}));
            }
        }
    }

    /// The channels a talk reaches.
    fn talk_channels(s: &Session, target: &Target) -> BTreeSet<u32> {
        match target {
            Target::Channel(c) => [*c].into(),
            Target::All => s
                .channels
                .iter()
                .filter(|(_, talk)| **talk)
                .map(|(c, _)| *c)
                .collect(),
            _ => BTreeSet::new(),
        }
    }

    // ---- talking

    /// Starts `id` talking into `target`.
    pub fn talk_start(&self, id: Sid, target: Target) -> Result<(), String> {
        let mut st = self.st.lock_ok();
        self.stop_talk_locked(&mut st, id, "replaced");
        let Some(s) = st.sessions.get(&id) else {
            return Err("gone".into());
        };
        let who = s.who.clone();
        let dispatch = who.admin || listed(&who, &st.settings.dispatch).is_some();
        let mut until = None;
        let mut users: Vec<String> = Vec::new();
        match &target {
            Target::Channel(c) => {
                if s.channels.get(c) != Some(&true) {
                    return Err("you can't talk in this channel".into());
                }
                if let Some(limit) = st.channels.get(c).and_then(Channel::half_duplex) {
                    if let Some(&holder) = st.floor.get(c)
                        && holder != id
                        && st.sessions.contains_key(&holder)
                    {
                        if !dispatch {
                            let by = st.sessions[&holder].who.name.clone();
                            send(s, json!({"t": "busy", "channel": c, "by": by}));
                            return Err("busy".into());
                        }
                        // Priority: the floor taken from the other.
                        self.stop_talk_locked(&mut st, holder, "taken");
                        if let Some(h) = st.sessions.get(&holder) {
                            send(h, json!({"t": "taken", "channel": c, "by": who.name}));
                        }
                    }
                    st.floor.insert(*c, id);
                    if limit > 0 {
                        until = Some(Instant::now() + Duration::from_secs(limit as u64));
                    }
                }
            }
            Target::All => {
                if !dispatch {
                    return Err("all-call is for dispatch".into());
                }
                let chans = Self::talk_channels(s, &target);
                for c in &chans {
                    if st.channels.get(c).and_then(Channel::half_duplex).is_some() {
                        if let Some(&holder) = st.floor.get(c)
                            && holder != id
                        {
                            self.stop_talk_locked(&mut st, holder, "taken");
                            if let Some(h) = st.sessions.get(&holder) {
                                send(h, json!({"t": "taken", "channel": c, "by": who.name}));
                            }
                        }
                        st.floor.insert(*c, id);
                    }
                }
            }
            Target::User(name) => {
                if !who.user {
                    return Err("guests can't talk to a person".into());
                }
                let me = who.key().to_owned();
                let answering = st
                    .answerable
                    .get(&(me.clone(), name.clone()))
                    .is_some_and(|t| t.elapsed() < ANSWER_WINDOW);
                if !answering {
                    let allowed = match self.store.user_settings(name).instant_from {
                        InstantFrom::Anyone => true,
                        InstantFrom::Dispatch => dispatch,
                        InstantFrom::Never => false,
                    };
                    if !allowed {
                        return Err(format!("{name} takes calls only"));
                    }
                }
                if !st.sessions.values().any(|x| x.who.answers_to(name)) {
                    return Err(format!("{name} isn't on"));
                }
                users.push(name.clone());
                st.answerable
                    .insert((name.clone(), me.clone()), Instant::now());
                for x in st.sessions.values().filter(|x| x.who.answers_to(name)) {
                    send(
                        x,
                        json!({"t": "instant", "from": me, "name": who.name, "on": true}),
                    );
                }
            }
            Target::Call(cid) => {
                let Some(c) = st.calls.get(cid) else {
                    return Err("no such call".into());
                };
                if c.b.is_none() || (c.a != id && c.b != Some(id)) {
                    return Err("not in that call".into());
                }
                let other = if c.a == id { c.b } else { Some(c.a) };
                if let Some(o) = other.and_then(|o| st.sessions.get(&o)) {
                    users.push(o.who.key().to_owned());
                }
            }
        }
        // Recordings: each recorded channel the talk reaches, or one for a person or call.
        let s = &st.sessions[&id];
        let mut recordings = Vec::new();
        let start_ms = now_ms();
        let chans = Self::talk_channels(s, &target);
        let mut rec_targets: Vec<(Option<u32>, Vec<String>)> = chans
            .iter()
            .filter(|c| st.channels.get(c).is_some_and(|c| c.record))
            .map(|c| (Some(*c), Vec::new()))
            .collect();
        if !users.is_empty() {
            rec_targets.push((None, users.clone()));
        }
        for (channel, to) in rec_targets {
            let Ok(tid) = self.store.next_transmission() else {
                continue;
            };
            let Ok(rec) = Recorder::create(&self.dir, tid) else {
                continue;
            };
            recordings.push(Recording {
                t: Transmission {
                    id: tid,
                    channel,
                    to,
                    from: who.name.clone(),
                    from_id: who.ident.clone(),
                    start_ms,
                    end_ms: start_ms,
                    frames: 0,
                    handset: who.handset.as_ref().map(|h| h.name.clone()),
                },
                rec,
            });
        }
        if let Some(s) = st.sessions.get_mut(&id) {
            s.talk = Some(Talk {
                target: target.clone(),
                started: Instant::now(),
                until,
                heard: None,
                recordings,
            });
        }
        Self::talkers_changed(&st, &chans);
        if let Some(s) = st.sessions.get(&id) {
            send(s, json!({"t": "talking", "on": true}));
        }
        let mut ev = json!({"event": "talk_start", "from": who.name, "target": target_json(&st, &target), "time_ms": start_ms});
        if let Some(h) = &who.handset {
            ev["handset"] = json!(h.name);
            ev["key"] = json!(who.key());
        }
        self.host.publish("events", &ev, false);
        Ok(())
    }

    pub fn talk_stop(&self, id: Sid) {
        let mut st = self.st.lock_ok();
        self.stop_talk_locked(&mut st, id, "released");
    }

    pub(super) fn stop_talk_locked(&self, st: &mut State, id: Sid, why: &str) {
        let Some(t) = st.sessions.get_mut(&id).and_then(|s| s.talk.take()) else {
            return;
        };
        let target = t.target.clone();
        let chans = st
            .sessions
            .get(&id)
            .map(|s| Self::talk_channels(s, &target))
            .unwrap_or_default();
        let name = st
            .sessions
            .get(&id)
            .map(|s| s.who.name.clone())
            .unwrap_or_default();
        let handset = st
            .sessions
            .get(&id)
            .and_then(|s| s.who.handset.as_ref().map(|h| h.name.clone()));
        let key = st
            .sessions
            .get(&id)
            .map(|s| s.who.key().to_owned())
            .unwrap_or_default();
        self.finish_talk(t);
        // Every floor it holds (an all-call's in a channel it has left since too).
        st.floor.retain(|_, holder| *holder != id);
        if let Target::User(n) = &target {
            for x in st.sessions.values().filter(|x| x.who.answers_to(n)) {
                send(
                    x,
                    json!({"t": "instant", "from": key, "name": name, "on": false}),
                );
            }
        }
        // Slots: the talker leaves every listener's slot.
        for s in st.sessions.values_mut() {
            for (k, slot) in s.slots.iter_mut().enumerate() {
                if *slot == Some(id) {
                    *slot = None;
                    let _ =
                        s.tx.send(Out::Json(json!({"t": "slot", "slot": k, "talker": null})));
                }
            }
        }
        Self::talkers_changed(st, &chans);
        if let Some(s) = st.sessions.get(&id) {
            send(s, json!({"t": "talking", "on": false, "why": why}));
        }
        let mut ev = json!({"event": "talk_end", "from": name, "target": target_json(st, &target), "why": why, "time_ms": now_ms()});
        if let Some(h) = handset {
            ev["handset"] = json!(h);
        }
        self.host.publish("events", &ev, false);
    }

    pub(super) fn finish_talk(&self, t: Talk) {
        let end = now_ms();
        for mut r in t.recordings {
            let frames = r.rec.frames;
            let _ = r.rec.finish();
            r.t.end_ms = end;
            r.t.frames = frames;
            if frames == 0 {
                crate::record::remove(&self.dir, r.t.id);
            } else {
                let _ = self.store.put_transmission(&r.t);
            }
        }
    }

    /// A talker's packet (from its WebSocket or WebRTC), to everyone it talks to.
    pub fn media_in(&self, id: Sid, data: &[u8]) {
        self.media_in_on(id, None, data)
    }

    /// [`media_in`](Self::media_in) from the WebSocket connection numbered `conn` (a
    /// connection a resumed handset has left talks for nobody).
    pub fn media_in_on(&self, id: Sid, conn: Option<u64>, data: &[u8]) {
        // One Opus packet (1275 bytes at most; room for a feed's): a 64 KiB "packet" was
        // copied to every listener and written to the recordings (bug round E, E5-2/3).
        if data.is_empty() || data.len() > MAX_PACKET {
            return;
        }
        let mut st = self.st.lock_ok();
        let Some(s) = st.sessions.get_mut(&id) else {
            return;
        };
        if conn.is_some_and(|c| c != s.conn) {
            return;
        }
        let Some(t) = s.talk.as_mut() else { return };
        t.heard = Some(Instant::now());
        for r in &mut t.recordings {
            let _ = r.rec.frame(data);
        }
        s.seq = s.seq.wrapping_add(1);
        let seq = s.seq;
        let target = t.target.clone();
        let iem = s.who.feed.is_some();
        let talker_name = s.who.name.clone();
        // Recipients: (session, channel shown).
        let mut to: Vec<(Sid, u32)> = Vec::new();
        match &target {
            Target::Channel(_) | Target::All => {
                let chans = Self::talk_channels(&st.sessions[&id], &target);
                let mut seen = BTreeSet::new();
                for (rid, r) in &st.sessions {
                    if *rid == id {
                        continue;
                    }
                    if let Some(c) = r.channels.keys().find(|c| chans.contains(c))
                        && seen.insert(*rid)
                    {
                        to.push((*rid, *c));
                    }
                }
            }
            Target::User(name) => {
                for (rid, r) in &st.sessions {
                    if *rid != id && r.who.answers_to(name) {
                        to.push((*rid, 0));
                    }
                }
            }
            Target::Call(cid) => {
                if let Some(c) = st.calls.get(cid) {
                    let other = if c.a == id { c.b } else { Some(c.a) };
                    if let Some(o) = other {
                        to.push((o, 0));
                    }
                }
            }
        }
        let rtc = self.rtc();
        let kind = if iem { 3 } else { 1 };
        let mut dropped = 0u64;
        // One copy of the packet for every listener's queue.
        let body: Arc<[u8]> = Arc::from(data);
        let (sf_on, live_keep) = (st.settings.store_forward, st.settings.ws_queue_s);
        for (rid, channel) in to {
            let window = st
                .channels
                .get(&channel)
                .and_then(|c| c.store_forward_s)
                .unwrap_or(st.settings.store_forward_s);
            let Some(r) = st.sessions.get_mut(&rid) else {
                continue;
            };
            if r.rtc
                && let Some(sink) = &rtc
            {
                // A slot for this talker: its own, else a free one (else not heard).
                let slot = match r.slots.iter().position(|x| *x == Some(id)) {
                    Some(k) => Some(k),
                    None => r.slots.iter().position(Option::is_none).inspect(|&k| {
                        r.slots[k] = Some(id);
                        let _ = r.tx.send(Out::Json(json!({
                            "t": "slot", "slot": k,
                            "talker": {"sid": id, "name": talker_name, "channel": channel, "iem": iem},
                        })));
                    }),
                };
                if let Some(k) = slot {
                    sink.send(rid, k, data);
                }
                continue;
            }
            // Kept for its window: a live link's slow-link queue; a handset's the longer
            // of that and the channel's store-and-forward, and kept after it is sent too
            // (a link that died without a word swallowed it; a resume sends it again);
            // a handset's link that is down, store-and-forward alone (none: not kept,
            // `missed` has it).
            let forward = sf_on && window > 0 && r.who.handset.is_some();
            let (keep, retain) = if r.away.is_some() {
                if !forward {
                    continue;
                }
                (window, true)
            } else if forward {
                (window.max(live_keep), true)
            } else {
                (live_keep, false)
            };
            dropped += r.frames.push(frames::Queued {
                kind,
                talker: id,
                channel,
                seq,
                body: body.clone(),
                at: Instant::now(),
                keep: Duration::from_secs(keep as u64),
                retain,
                sent: false,
            });
        }
        st.frames_dropped += dropped;
    }
}

fn target_json(st: &State, t: &Target) -> Value {
    match t {
        Target::Channel(c) => json!({"channel": st.channels.get(c).map(|c| c.name.clone())}),
        Target::All => json!({"all": true}),
        Target::User(n) => json!({"user": n}),
        Target::Call(c) => json!({"call": c}),
    }
}
