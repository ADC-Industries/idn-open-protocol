//! Calls: ringing, answering, hanging up.

use super::*;

impl Hub {
    // ---- calls

    pub fn call(&self, id: Sid, user: &str) -> Result<u64, String> {
        let mut st = self.st.lock_ok();
        let Some(me) = st.sessions.get(&id) else {
            return Err("gone".into());
        };
        if !me.who.user {
            return Err("guests can't call".into());
        }
        if me.who.key() == user {
            return Err("that's you".into());
        }
        // (People by their key: a site user's name, a handset's `driver:…`, §V.13.3.)
        let from = me.who.key().to_owned();
        let from_name = me.who.name.clone();
        let theirs: Vec<Sid> = st
            .sessions
            .iter()
            .filter(|(_, s)| s.who.answers_to(user))
            .map(|(i, _)| *i)
            .collect();
        if theirs.is_empty() {
            return Err(format!("{user} isn't on"));
        }
        let cid = st.next_call;
        st.next_call += 1;
        st.calls.insert(
            cid,
            Call {
                a: id,
                b_name: user.to_owned(),
                b: None,
                since: Instant::now(),
            },
        );
        for t in theirs {
            send(
                &st.sessions[&t],
                json!({"t": "ring", "call": cid, "from": from, "name": from_name}),
            );
        }
        send(
            &st.sessions[&id],
            json!({"t": "call", "call": cid, "state": "ringing", "with": user}),
        );
        self.host
            .audit(&from_name, "voice.call", json!({"from": from, "to": user}));
        Ok(cid)
    }

    pub fn answer(&self, id: Sid, cid: u64, accept: bool) -> Result<(), String> {
        let mut st = self.st.lock_ok();
        let Some(me) = st.sessions.get(&id).map(|s| s.who.clone()) else {
            return Err("gone".into());
        };
        let Some(c) = st.calls.get_mut(&cid) else {
            return Err("no such call".into());
        };
        if !me.answers_to(&c.b_name) || c.b.is_some() {
            return Err("not your call".into());
        }
        if !accept {
            self.end_call_locked(&mut st, cid, "declined");
            return Ok(());
        }
        c.b = Some(id);
        let (a, b_name) = (c.a, c.b_name.clone());
        let a_name = st
            .sessions
            .get(&a)
            .map(|s| s.who.key().to_owned())
            .unwrap_or_default();
        // The callee's other sessions stop ringing.
        for (sid, s) in &st.sessions {
            if s.who.answers_to(&b_name) && *sid != id {
                send(
                    s,
                    json!({"t": "call", "call": cid, "state": "answered elsewhere", "with": a_name}),
                );
            }
        }
        if let Some(s) = st.sessions.get(&a) {
            send(
                s,
                json!({"t": "call", "call": cid, "state": "answered", "with": b_name}),
            );
        }
        if let Some(s) = st.sessions.get(&id) {
            send(
                s,
                json!({"t": "call", "call": cid, "state": "answered", "with": a_name}),
            );
        }
        self.host.audit(
            &me.name,
            "voice.answer",
            json!({"by": b_name, "from": a_name}),
        );
        Ok(())
    }

    pub fn hangup(&self, id: Sid, cid: u64) {
        let mut st = self.st.lock_ok();
        if st.calls.get(&cid).is_some_and(|c| {
            c.a == id
                || c.b == Some(id)
                || st
                    .sessions
                    .get(&id)
                    .is_some_and(|s| s.who.answers_to(&c.b_name))
        }) {
            self.end_call_locked(&mut st, cid, "hung up");
        }
    }

    pub(super) fn end_call_locked(&self, st: &mut State, cid: u64, why: &str) {
        let Some(c) = st.calls.remove(&cid) else {
            return;
        };
        let a_name = st
            .sessions
            .get(&c.a)
            .map(|s| s.who.key().to_owned())
            .unwrap_or_default();
        // Whoever was talking into it stops.
        for sid in [Some(c.a), c.b].into_iter().flatten() {
            if let Some(t) = st
                .sessions
                .get_mut(&sid)
                .and_then(|s| s.talk.take_if(|t| t.target == Target::Call(cid)))
            {
                self.finish_talk(t);
            }
        }
        let missed = c.b.is_none() && why != "declined";
        for (_, s) in st.sessions.iter().filter(|(sid, s)| {
            **sid == c.a || Some(**sid) == c.b || (c.b.is_none() && s.who.answers_to(&c.b_name))
        }) {
            let with = if s.who.answers_to(&c.b_name) {
                &a_name
            } else {
                &c.b_name
            };
            send(
                s,
                json!({"t": "call", "call": cid, "state": "ended", "why": why, "with": with, "missed": missed}),
            );
        }
    }
}
