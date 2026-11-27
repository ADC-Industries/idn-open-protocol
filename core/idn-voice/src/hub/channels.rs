//! Channels: their administration (made, changed, removed), joining and leaving, and member lists.

use super::*;

impl Hub {
    // ---- administration

    /// A channel made or changed: members who may no longer be in it are taken out.
    pub fn put_channel(&self, c: Channel) -> std::io::Result<Channel> {
        let c = self.store.put_channel(c)?;
        let mut st = self.st.lock_ok();
        let before = st.channels.insert(c.id, c.clone());
        // A password channel keeps who came in with its password while that password
        // stays; a password set or changed, or access changed away from it, takes out
        // whoever isn't let in otherwise.
        let pw_ok = matches!(
            (before.as_ref().map(|b| &b.access), &c.access),
            (Some(Access::Password { hash: a }), Access::Password { hash: b }) if a == b
        );
        let out: Vec<Sid> = st
            .sessions
            .iter()
            .filter(|(_, s)| s.channels.contains_key(&c.id))
            .filter(|(_, s)| may(&s.who, &c, pw_ok).is_none())
            .map(|(id, _)| *id)
            .collect();
        for id in out {
            self.leave_locked(&mut st, id, c.id, "access changed");
        }
        // Talk rights may have changed: each member's own.
        let ids: Vec<Sid> = st
            .sessions
            .iter()
            .filter(|(_, s)| s.channels.contains_key(&c.id))
            .map(|(i, _)| *i)
            .collect();
        for id in ids {
            let talk = may(&st.sessions[&id].who, &c, true).unwrap_or(false);
            let talking_there = st.sessions[&id]
                .talk
                .as_ref()
                .is_some_and(|t| t.target == Target::Channel(c.id));
            if let Some(s) = st.sessions.get_mut(&id) {
                s.channels.insert(c.id, talk);
            }
            // Talk taken away (or the channel made IEM): a talk there ends now.
            if !talk && talking_there {
                self.stop_talk_locked(&mut st, id, "access changed");
            }
        }
        Self::broadcast_channels(&st);
        drop(st);
        // A channel renamed (or made stricter) is in a driver's assignment, or no longer.
        self.regrant();
        Ok(c)
    }

    pub fn remove_channel(&self, id: u32) -> std::io::Result<bool> {
        let had = self.store.remove_channel(id)?;
        let mut st = self.st.lock_ok();
        let ids: Vec<Sid> = st
            .sessions
            .iter()
            .filter(|(_, s)| s.channels.contains_key(&id))
            .map(|(i, _)| *i)
            .collect();
        for s in ids {
            self.leave_locked(&mut st, s, id, "channel removed");
        }
        st.channels.remove(&id);
        st.floor.remove(&id);
        Self::broadcast_channels(&st);
        drop(st);
        self.regrant();
        Ok(had)
    }

    pub fn put_settings(&self, s: Settings) -> std::io::Result<()> {
        s.check().map_err(idn_common::err::other)?;
        self.store.put_settings(&s)?;
        self.budget.set_limit(s.frame_budget_mb as u64 * 1_000_000);
        let link = s.link;
        self.st.lock_ok().settings = s;
        // `link` turned off: the sessions on the link end (§V.14).
        self.link_on.send_replace(link);
        Ok(())
    }

    pub(super) fn channel_list(st: &State, who: &Who) -> Value {
        let list: Vec<Value> = st
            .channels
            .values()
            .filter_map(|c| {
                // Password channels are listed for everyone who could enter them that way.
                let talk = may(who, c, true)?;
                let mut v = c.public();
                v["may_talk"] = json!(talk);
                v["jitter_start_ms"] =
                    json!(c.jitter_start_ms.unwrap_or(st.settings.jitter_start_ms));
                v["jitter_max_ms"] = json!(c.jitter_max_ms.unwrap_or(st.settings.jitter_max_ms));
                v["needs_password"] = json!(
                    matches!(c.access, Access::Password { .. })
                        && !who.admin
                        && !who.granted.contains(&c.id)
                );
                Some(v)
            })
            .collect();
        json!(list)
    }

    fn broadcast_channels(st: &State) {
        for s in st.sessions.values() {
            send(
                s,
                json!({"t": "channels", "channels": Self::channel_list(st, &s.who)}),
            );
        }
    }

    // ---- channels

    pub fn join(&self, id: Sid, channel: u32, password: Option<&str>) -> Result<(), String> {
        // The password checked with the hub let go (Argon2: tens of ms; under the lock,
        // every session's sound waited, and a guest's tries stopped all talk; bug round E,
        // E5-1).
        let hash = match self
            .st
            .lock_ok()
            .channels
            .get(&channel)
            .map(|c| c.access.clone())
        {
            Some(Access::Password { hash }) => Some(hash),
            Some(_) => None,
            None => return Err("no such channel".into()),
        };
        let pw_given_ok = match (&hash, password) {
            (Some(h), Some(p)) => idn_open_protocol::web::verify_password(h, p),
            _ => false,
        };
        let mut st = self.st.lock_ok();
        let pw_ok = match st.channels.get(&channel).map(|c| &c.access) {
            // (The same password as checked: changed meanwhile, checked again next time.)
            Some(Access::Password { hash: now }) => pw_given_ok && hash.as_ref() == Some(now),
            _ => true,
        };
        self.join_locked(&mut st, id, channel, pw_ok).map(|_| ())
    }

    /// `id` into `channel`, `pw_ok` whether its password counts as given: whether it may
    /// talk there.
    pub(super) fn join_locked(
        &self,
        st: &mut State,
        id: Sid,
        channel: u32,
        pw_ok: bool,
    ) -> Result<bool, String> {
        let Some(c) = st.channels.get(&channel).cloned() else {
            return Err("no such channel".into());
        };
        let Some(s) = st.sessions.get(&id) else {
            return Err("gone".into());
        };
        let Some(talk) = may(&s.who, &c, pw_ok || s.who.admin) else {
            return Err(if matches!(c.access, Access::Password { .. }) {
                "wrong password".into()
            } else {
                "not allowed in this channel".into()
            });
        };
        let members = st
            .sessions
            .values()
            .filter(|x| x.channels.contains_key(&channel))
            .count();
        if members >= MAX_MEMBERS {
            return Err("the channel is full".into());
        }
        if let Some(s) = st.sessions.get_mut(&id) {
            s.channels.insert(channel, talk);
        }
        Self::members_changed(st, channel);
        // The one joining has the list at once; the others at the next tick.
        if let Some(s) = st.sessions.get(&id) {
            send(s, Self::members_message(st, channel));
        }
        let talkers = Self::talkers(st, channel);
        if let Some(s) = st.sessions.get(&id) {
            send(
                s,
                json!({"t": "joined", "channel": channel, "may_talk": talk}),
            );
            send(
                s,
                json!({"t": "talkers", "channel": channel, "talkers": talkers}),
            );
        }
        Ok(talk)
    }

    /// Every few seconds, the signed-in sessions as the site has their users now: one
    /// removed is closed, one whose roles changed has the new ones (and leaves channels it
    /// may no longer be in). A socket kept its admin and roles from its start for good,
    /// whatever the site did (bug round E, E5-5).
    pub(super) fn follow_users(&self) {
        {
            let mut t = self.users_checked.lock_ok();
            if t.elapsed() < Duration::from_secs(5) {
                return;
            }
            *t = Instant::now();
        }
        let users = self.host.users();
        let mut st = self.st.lock_ok();
        // (Handsets aren't site accounts: their identity follows their record, §V.13. The
        // site's program isn't one either: its credential is checked at sign-in.)
        let ids: Vec<Sid> = st
            .sessions
            .iter()
            .filter(|(_, s)| {
                s.who.user && s.who.feed.is_none() && s.who.handset.is_none() && !s.who.api
            })
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let name = st.sessions[&id].who.name.clone();
            match users.iter().find(|u| u.name == name) {
                None => {
                    let _ = st.sessions[&id]
                        .tx
                        .send(Out::Close("your account was removed".into()));
                }
                Some(u) => {
                    let s = st.sessions.get_mut(&id).expect("listed");
                    if s.who.admin == u.admin && s.who.roles == u.roles {
                        continue;
                    }
                    s.who.admin = u.admin;
                    s.who.roles = u.roles.clone();
                    let who = s.who.clone();
                    let mine: Vec<u32> = s.channels.keys().copied().collect();
                    // Each channel it is in, as put_channel does: in a password channel
                    // it came in with the password (kept while that stays), and talking
                    // there is what its roles allow now (bug run 2026-11-24: a role
                    // change kicked password members, and a talk right taken away
                    // stayed).
                    for c in mine {
                        let now = st.channels.get(&c).and_then(|ch| may(&who, ch, true));
                        let Some(talk) = now else {
                            self.leave_locked(&mut st, id, c, "your role changed");
                            continue;
                        };
                        let talking_there = st.sessions[&id]
                            .talk
                            .as_ref()
                            .is_some_and(|t| t.target == Target::Channel(c));
                        if let Some(s) = st.sessions.get_mut(&id) {
                            s.channels.insert(c, talk);
                        }
                        if !talk && talking_there {
                            self.stop_talk_locked(&mut st, id, "your role changed");
                        }
                    }
                }
            }
        }
    }

    pub fn leave(&self, id: Sid, channel: u32) {
        let mut st = self.st.lock_ok();
        self.leave_locked(&mut st, id, channel, "left");
    }

    pub(super) fn leave_locked(&self, st: &mut State, id: Sid, channel: u32, why: &str) {
        let talking_there = st.sessions.get(&id).is_some_and(|s| {
            s.talk
                .as_ref()
                .is_some_and(|t| t.target == Target::Channel(channel))
        });
        if talking_there {
            self.stop_talk_locked(st, id, why);
        }
        // Talking on elsewhere (an all-call): this channel's floor is let go now.
        if st.floor.get(&channel) == Some(&id) {
            st.floor.remove(&channel);
        }
        let Some(s) = st.sessions.get_mut(&id) else {
            return;
        };
        if s.channels.remove(&channel).is_none() {
            return;
        }
        let s = &st.sessions[&id];
        send(s, json!({"t": "left", "channel": channel, "why": why}));
        Self::members_changed(st, channel);
    }

    fn members_changed(st: &mut State, channel: u32) {
        st.members_dirty.insert(channel);
    }

    pub(super) fn members_message(st: &State, channel: u32) -> Value {
        let names: Vec<&str> = st
            .sessions
            .values()
            .filter(|s| s.channels.contains_key(&channel) && s.who.feed.is_none())
            .map(|s| s.who.name.as_str())
            .collect();
        json!({"t": "members", "channel": channel, "members": names})
    }

    /// The changed channels' member lists, to their members.
    pub(super) fn send_members(st: &mut State) {
        for channel in std::mem::take(&mut st.members_dirty) {
            let m = Self::members_message(st, channel);
            for s in st
                .sessions
                .values()
                .filter(|s| s.channels.contains_key(&channel))
            {
                send(s, m.clone());
            }
        }
    }
}
