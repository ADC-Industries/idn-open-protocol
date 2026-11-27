//! Sessions: connecting, resuming, disconnecting, and a session's identity.

use super::*;

impl Hub {
    // ---- sessions

    /// A session connected: its number, after the `hello` is queued, and the queue its
    /// WebSocket's sound comes through. `None`: a handset whose credential stopped being
    /// its own since it was checked (revoked, enrolled again).
    pub fn connect(
        &self,
        who: Who,
        tx: tokio::sync::mpsc::UnboundedSender<Out>,
    ) -> Option<(Sid, Arc<FrameQueue>)> {
        let dispatch = self.dispatch(&who);
        let settings = if who.user {
            self.store.user_settings(who.key())
        } else {
            UserSettings::default()
        };
        // A handset's record checked again with the session made under the same locks: a
        // revoke in between either comes first (refused here) or finds the session.
        let hs = self.hs.lock_ok();
        if let Some(h) = &who.handset
            && !hs
                .get(&h.id)
                .is_some_and(|r| !r.revoked && r.secret_sha256 == h.secret)
        {
            return None;
        }
        let mut st = self.st.lock_ok();
        drop(hs);
        let id = st.next_sid;
        st.next_sid = st.next_sid.wrapping_add(1).max(1);
        let frames = Arc::new(FrameQueue::new(self.budget.clone()));
        let resume = who.handset.as_ref().map(|_| handsets::resume_token());
        let s = Session {
            who: who.clone(),
            tx,
            frames: frames.clone(),
            conn: 0,
            away: None,
            resume: resume.iter().cloned().collect(),
            by_driver: BTreeSet::new(),
            channels: BTreeMap::new(),
            rtc: false,
            talk: None,
            seq: 0,
            slots: [None; SLOTS],
            sent: Default::default(),
        };
        let mut hello = Self::hello(&st, id, &who, &settings, dispatch);
        if let Some(r) = resume {
            hello["resume"] = json!(r);
            hello["resumed"] = json!(false);
        }
        send(&s, hello);
        // A dispatcher has the SOS alerts not yet acknowledged at once (§V.13).
        if dispatch {
            for a in st.alerts.values() {
                send(&s, json!({"t": "sos", "sos": a.json()}));
            }
        }
        st.sessions.insert(id, s);
        if who.user && who.feed.is_none() {
            self.broadcast_presence(&st);
        }
        Some((id, frames))
    }

    /// Whether `id` is still a session on its connection number `conn` (a handset that
    /// resumed elsewhere leaves its old connection acting for nobody).
    pub fn is_current(&self, id: Sid, conn: u64) -> bool {
        self.st
            .lock_ok()
            .sessions
            .get(&id)
            .is_some_and(|s| s.conn == conn)
    }

    pub fn disconnect(&self, id: Sid) {
        let mut st = self.st.lock_ok();
        self.drop_session_locked(&mut st, id);
        drop(st);
        if let Some(r) = self.rtc() {
            r.forget(id);
        }
    }

    /// A session ended: its talk, channels and calls, then the session itself (its WebRTC
    /// is the caller's to forget, with the lock let go).
    pub(super) fn drop_session_locked(&self, st: &mut State, id: Sid) {
        self.stop_talk_locked(st, id, "left");
        let chans: Vec<u32> = st
            .sessions
            .get(&id)
            .map(|s| s.channels.keys().copied().collect())
            .unwrap_or_default();
        for c in chans {
            self.leave_locked(st, id, c, "left");
        }
        let calls: Vec<u64> = st
            .calls
            .iter()
            .filter(|(_, c)| c.a == id || c.b == Some(id))
            .map(|(k, _)| *k)
            .collect();
        for c in calls {
            self.end_call_locked(st, c, "hung up");
        }
        let was_user = st
            .sessions
            .remove(&id)
            .is_some_and(|s| s.who.user && s.who.feed.is_none());
        if was_user {
            self.broadcast_presence(st);
        }
    }

    pub fn set_rtc_media(&self, id: Sid, on: bool) {
        if let Some(s) = self.st.lock_ok().sessions.get_mut(&id) {
            s.rtc = on;
            s.slots = [None; SLOTS];
        }
    }

    pub fn who(&self, id: Sid) -> Option<Who> {
        self.st.lock_ok().sessions.get(&id).map(|s| s.who.clone())
    }
}
