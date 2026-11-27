//! Screen sessions (§I.11.3, §I.11.4): started and stopped, their viewers, the
//! machine's pictures relayed to them, their input and chat forwarded.

use super::*;

impl SiteManager {
    /// Starts a screen session on a connected machine: registers it, asks the machine to
    /// capture, and returns (session id, width, height).
    pub async fn start_screen(
        &self,
        fp: &str,
        params: serde_json::Value,
        user: &str,
    ) -> io::Result<(String, u64, u64)> {
        let session = idn_open_protocol::random::token_hex(16);
        let (frames, _) = broadcast::channel(16);
        let control = params
            .get("control")
            .and_then(|c| c.as_bool())
            .unwrap_or(false);
        let notify = params
            .get("notify")
            .and_then(|n| n.as_bool())
            .unwrap_or(true);
        let sess = Arc::new(ScreenSession {
            fp: fp.to_owned(),
            frames,
            last_keyframe: Mutex::new(None),
            control,
            user: user.to_owned(),
            started_ms: wire::now_ms(),
            viewers: std::sync::atomic::AtomicUsize::new(0),
        });
        self.0.screens.lock_ok().insert(session.clone(), sess);
        let mut body = params;
        if let Some(o) = body.as_object_mut() {
            o.insert("session".into(), session.clone().into());
            o.insert("user".into(), user.into());
            o.insert("notify".into(), notify.into());
        }
        let r = self
            .command(
                fp,
                "remote.screen.start",
                serde_json::to_vec(&body).unwrap_or_default(),
                Duration::from_secs(15),
            )
            .await;
        match r {
            Ok((wire::ST_OK, p)) => {
                let j: serde_json::Value = serde_json::from_slice(&p).unwrap_or_default();
                let w = j.get("width").and_then(|v| v.as_u64()).unwrap_or(0);
                let h = j.get("height").and_then(|v| v.as_u64()).unwrap_or(0);
                let machine = self.machine(fp).map(|m| m.topic).unwrap_or_default();
                self.audit(
                    user,
                    &machine,
                    "session.start",
                    serde_json::json!({ "session": session, "control": control, "notify": notify }),
                );
                // A session no browser opens (a blocked pop-up, an API client that only
                // starts) ends: the machine would capture and send for nobody.
                let (me, id) = (self.clone(), session.clone());
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    let unwatched =
                        me.0.screens.lock_ok().get(&id).is_some_and(|s| {
                            s.viewers.load(std::sync::atomic::Ordering::SeqCst) == 0
                        });
                    if unwatched {
                        let _ = me.stop_screen(&id, "no viewer").await;
                    }
                });
                Ok((session, w, h))
            }
            Ok((_, p)) => {
                self.0.screens.lock_ok().remove(&session);
                Err(io::Error::other(String::from_utf8_lossy(&p).into_owned()))
            }
            Err(e) => {
                self.0.screens.lock_ok().remove(&session);
                Err(e)
            }
        }
    }

    /// Ends a screen session (and tells the machine).
    pub async fn stop_screen(&self, session: &str, user: &str) -> io::Result<()> {
        self.stop_screen_as(session, user, true).await
    }

    /// [`stop_screen`](Self::stop_screen) asked by `user`: another's session only when
    /// `others` (an operator; a viewer listed every session and ended an operator's control
    /// session mid-work; bug review 3).
    pub async fn stop_screen_as(&self, session: &str, user: &str, others: bool) -> io::Result<()> {
        let sess = {
            let mut screens = self.0.screens.lock_ok();
            match screens.get(session) {
                Some(s) if !others && s.user != user => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "another user's session: an operator ends it",
                    ));
                }
                _ => screens.remove(session),
            }
        };
        if let Some(sess) = sess {
            let machine = self.machine(&sess.fp).map(|m| m.topic).unwrap_or_default();
            let _ = self
                .command(
                    &sess.fp,
                    "remote.screen.stop",
                    serde_json::json!({ "session": session })
                        .to_string()
                        .into_bytes(),
                    Duration::from_secs(5),
                )
                .await;
            self.audit(
                user,
                &machine,
                "session.end",
                serde_json::json!({ "session": session }),
            );
        }
        Ok(())
    }

    /// What a browser needs to watch a session: its frames, the latest keyframe, whether
    /// it allows control, and the machine's fingerprint.
    pub fn screen_watch(&self, session: &str) -> Option<ScreenWatch> {
        let s = self.0.screens.lock_ok();
        let sess = s.get(session)?;
        Some((
            sess.frames.subscribe(),
            sess.last_keyframe.lock_ok().clone(),
            sess.control,
            sess.fp.clone(),
        ))
    }

    /// A browser started watching `session`: count it so the session can end when the
    /// last viewer leaves.
    pub fn viewer_joined(&self, session: &str) {
        if let Some(s) = self.0.screens.lock_ok().get(session) {
            s.viewers.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// A browser stopped watching `session`; if it was the last, the session ends and the
    /// machine stops capturing (§I.11.3).
    pub async fn viewer_left(&self, session: &str) {
        let last = {
            let screens = self.0.screens.lock_ok();
            match screens.get(session) {
                // fetch_sub returns the previous count; 1 means this was the last viewer.
                Some(s) => s.viewers.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) == 1,
                None => false,
            }
        };
        if last {
            let _ = self.stop_screen(session, "viewer left").await;
        }
    }

    /// The running sessions (for the UI).
    pub fn screens(&self) -> Vec<serde_json::Value> {
        self.0
            .screens
            .lock_ok()
            .iter()
            .map(|(id, s)| {
                serde_json::json!({
                    "session": id,
                    "fp": s.fp,
                    "machine": self.machine(&s.fp).map(|m| m.topic),
                    "user": s.user,
                    "control": s.control,
                    "started_ms": s.started_ms,
                })
            })
            .collect()
    }

    /// Reads a machine's screen stream and fans the pictures out to the browsers watching
    /// the session (§I.11.3): only a session on machine `fp`. When the stream ends, so
    /// does the session (its viewers see it end instead of waiting).
    pub(super) async fn relay_screen(&self, fp: &str, session: String, r: crate::transport::Recv) {
        let ours = self
            .0
            .screens
            .lock_ok()
            .get(&session)
            .is_some_and(|s| s.fp == fp);
        if !ours {
            return;
        }
        self.relay_pictures(&session, r).await;
        if self.0.screens.lock_ok().contains_key(&session) {
            let _ = self
                .stop_screen(&session, "the machine's stream ended")
                .await;
        }
    }

    async fn relay_pictures(&self, session: &str, mut r: crate::transport::Recv) {
        use tokio::io::AsyncReadExt;
        loop {
            let mut head = [0u8; 13];
            if r.read_exact(&mut head).await.is_err() {
                break;
            }
            let keyframe = head[0] != 0;
            let len = u32::from_le_bytes([head[9], head[10], head[11], head[12]]) as usize;
            if len > 64 << 20 {
                break;
            }
            let mut data = vec![0u8; len];
            if r.read_exact(&mut data).await.is_err() {
                break;
            }
            // Browser record: keyframe flag, pts (from the machine), then the H.264.
            let mut rec = Vec::with_capacity(9 + len);
            rec.push(head[0]);
            rec.extend_from_slice(&head[1..9]);
            rec.extend_from_slice(&data);
            let rec = Arc::new(rec);
            let sessions = self.0.screens.lock_ok();
            let Some(sess) = sessions.get(session) else {
                break;
            };
            if keyframe {
                *sess.last_keyframe.lock_ok() = Some(rec.clone());
            }
            let _ = sess.frames.send(rec);
        }
    }

    /// Forwards a viewer's input to the machine (§I.11.4); returns the clipboard text if
    /// one was asked for.
    pub async fn send_input(
        &self,
        session: &str,
        events: serde_json::Value,
    ) -> io::Result<serde_json::Value> {
        let fp = self
            .0
            .screens
            .lock_ok()
            .get(session)
            .map(|s| s.fp.clone())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such session"))?;
        let body = serde_json::json!({ "session": session, "events": events });
        let (st, p) = self
            .command(
                &fp,
                "remote.input",
                serde_json::to_vec(&body).unwrap_or_default(),
                Duration::from_secs(5),
            )
            .await?;
        if st == wire::ST_OK {
            Ok(serde_json::from_slice(&p).unwrap_or(serde_json::json!({})))
        } else {
            Err(io::Error::other(String::from_utf8_lossy(&p).into_owned()))
        }
    }

    /// Sends a chat message to a machine's operator (§I.11.5).
    pub async fn send_chat(&self, fp: &str, from: &str, text: &str) -> io::Result<()> {
        let body = serde_json::json!({ "from": from, "text": text });
        let (st, p) = self
            .command(
                fp,
                "remote.chat",
                serde_json::to_vec(&body).unwrap_or_default(),
                Duration::from_secs(10),
            )
            .await?;
        let machine = self.machine(fp).map(|m| m.topic).unwrap_or_default();
        self.audit(
            from,
            &machine,
            "chat",
            serde_json::json!({ "text": text, "to_station": true }),
        );
        if st == wire::ST_OK {
            Ok(())
        } else {
            Err(io::Error::other(String::from_utf8_lossy(&p).into_owned()))
        }
    }
}
