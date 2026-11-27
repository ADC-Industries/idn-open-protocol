//! Presence and status: who is on, the `hello` a session is sent, user settings, the hub's status.

use super::*;

impl Hub {
    /// The site users on (handsets are in [`Self::handsets_on`], by their key).
    fn presence(st: &State) -> Vec<String> {
        let set: BTreeSet<&str> = st
            .sessions
            .values()
            .filter(|s| s.who.user && s.who.feed.is_none() && s.who.handset.is_none())
            .map(|s| s.who.name.as_str())
            .collect();
        set.into_iter().map(str::to_owned).collect()
    }

    /// The handsets on now, each with its driver (§V.13).
    fn handsets_on(st: &State) -> Vec<Value> {
        let mut seen = BTreeSet::new();
        st.sessions
            .values()
            .filter_map(|s| {
                let h = s.who.handset.as_ref()?;
                seen.insert(h.id.as_str()).then(|| {
                    json!({
                        "id": h.id, "name": h.name, "key": s.who.key(),
                        "driver": h.driver.as_ref().map(|d| json!({"id": d, "name": s.who.name})),
                        "state": if s.away.is_some() { "reconnecting" } else { "on" },
                    })
                })
            })
            .collect()
    }

    pub(super) fn broadcast_presence(&self, st: &State) {
        let p = Self::presence(st);
        let h = Self::handsets_on(st);
        // Signed-in sessions only: a guest (no account) learnt the site's account names
        // as they came and went (bug run 2026-11-25; the HTTP lists are admins' already).
        for s in st.sessions.values().filter(|s| s.who.user) {
            send(s, json!({"t": "presence", "users": p, "handsets": h}));
        }
        self.host.publish(
            "presence",
            &json!({"users": p, "handsets": h, "time_ms": now_ms()}),
            true,
        );
    }

    /// The `hello` a session is sent on connecting, and again when its identity changes
    /// (a handset's driver, §V.13).
    pub(super) fn hello(
        st: &State,
        id: Sid,
        who: &Who,
        settings: &UserSettings,
        dispatch: bool,
    ) -> Value {
        let joined: Vec<Value> = st
            .sessions
            .get(&id)
            .map(|s| {
                s.channels
                    .iter()
                    .map(|(c, talk)| json!({"channel": c, "may_talk": talk}))
                    .collect()
            })
            .unwrap_or_default();
        json!({
            "t": "hello", "sid": id,
            "me": {
                "name": who.name, "key": who.key(), "user": who.user, "admin": who.admin,
                "dispatch": dispatch, "roles": who.roles,
                "handset": who.handset.as_ref().map(|h| json!({"id": h.id, "name": h.name})),
                "driver": who.handset.as_ref().and_then(|h| h.driver.as_ref())
                    .map(|d| json!({"id": d, "name": who.name})),
            },
            "channels": Self::channel_list(st, who),
            "joined": joined,
            "users": if who.user { Self::presence(st) } else { Vec::new() },
            "settings": settings,
            "slots": SLOTS,
            "link": Self::link_block(st),
            // A handset uses the WebSocket for its sound (reliable, over TCP): the server
            // never steers it to WebRTC (§V.13).
            "media": if who.handset.is_some() { "websocket" } else { "any" },
        })
    }

    /// `hello`'s `link`: the site's timings, and where Voice's QUIC is (§V.14), `null`
    /// where it isn't offered.
    fn link_block(st: &State) -> Value {
        let mut l = st.settings.link();
        l["quic"] = match &st.link_offer {
            Some(o) if st.settings.link => json!(o),
            _ => Value::Null,
        };
        l
    }

    // ---- settings

    pub fn set_user_settings(&self, id: Sid, s: UserSettings) -> Result<(), String> {
        let who = self.who(id).ok_or("gone")?;
        if !who.user {
            return Err("guests have no settings".into());
        }
        self.store
            .put_user_settings(who.key(), &s)
            .map_err(|e| e.to_string())
    }

    pub fn status(&self) -> Value {
        let st = self.st.lock_ok();
        json!({
            "sessions": st.sessions.len(),
            "users": Self::presence(&st),
            "talking": st.sessions.values().filter(|s| s.talk.is_some()).count(),
            "calls": st.calls.len(),
            "channels": st.channels.len(),
            "handsets": Self::handsets_on(&st),
            "frames_dropped": st.frames_dropped,
            "frame_bytes": self.budget.used(),
            "budget_dropped": self.budget.dropped.load(std::sync::atomic::Ordering::Relaxed),
            "sos": st.alerts.values().map(handsets::Alert::json).collect::<Vec<_>>(),
        })
    }
}
