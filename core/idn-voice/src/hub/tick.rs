//! The hub's quarter-second tick: half-duplex limits, the longest talk, unanswered calls.

use super::*;

impl Hub {
    // ---- time

    /// Every quarter second: half-duplex limits, the longest talk, unanswered calls.
    pub fn tick(&self) {
        self.follow_users();
        let mut st = self.st.lock_ok();
        let now = Instant::now();
        let silence = Duration::from_secs(st.settings.talk_silence_s as u64);
        let first = Duration::from_secs(st.settings.talk_first_s as u64);
        let resume_for = Duration::from_secs(st.settings.resume_s as u64);
        let over: Vec<(Sid, &'static str)> = st
            .sessions
            .iter()
            .filter_map(|(id, s)| {
                let t = s.talk.as_ref()?;
                let quiet = match t.heard {
                    Some(h) => now.duration_since(h) > silence,
                    None => t.started.elapsed() > first,
                };
                if t.until.is_some_and(|u| now >= u) {
                    Some((*id, "time limit"))
                } else if quiet && s.who.feed.is_none() {
                    // (A program's talk is its own: an IEM feed's source may go quiet.)
                    Some((*id, "no sound"))
                } else if t.started.elapsed() > MAX_TALK && s.who.program != Program::Feed {
                    // (An IEM channel's feed talks for good: it started once, at its
                    // connection, and nothing would start it again.)
                    Some((*id, "too long"))
                } else {
                    None
                }
            })
            .collect();
        for (id, why) in over {
            self.stop_talk_locked(&mut st, id, why);
        }
        // An IEM feed recorded talks for good: its recording ended and started again every
        // ten minutes, so each is indexed (and kept within bounds; E5-11).
        let roll: Vec<(Sid, Target)> = st
            .sessions
            .iter()
            .filter_map(|(id, s)| {
                let t = s.talk.as_ref()?;
                (s.who.program == Program::Feed
                    && !t.recordings.is_empty()
                    && t.started.elapsed() > MAX_TALK)
                    .then(|| (*id, t.target.clone()))
            })
            .collect();
        for (id, _) in &roll {
            self.stop_talk_locked(&mut st, *id, "a new recording");
        }
        let unanswered: Vec<u64> = st
            .calls
            .iter()
            .filter(|(_, c)| c.b.is_none() && c.since.elapsed() > RING_FOR)
            .map(|(k, _)| *k)
            .collect();
        for c in unanswered {
            self.end_call_locked(&mut st, c, "no answer");
        }
        st.answerable.retain(|_, t| t.elapsed() < ANSWER_WINDOW);
        // Handsets away past their resume window: ended now (§V.13).
        let gone: Vec<Sid> = st
            .sessions
            .iter()
            .filter(|(_, s)| s.away.is_some_and(|a| a.elapsed() > resume_for))
            .map(|(id, _)| *id)
            .collect();
        for &id in &gone {
            self.drop_session_locked(&mut st, id);
        }
        self.sos_realert_locked(&mut st);
        self.enrols.lock_ok().prune();
        Self::send_members(&mut st);
        drop(st);
        for (id, target) in roll {
            let _ = self.talk_start(id, target);
        }
        if let Some(r) = self.rtc() {
            for id in gone {
                r.forget(id);
            }
        }
    }
}
