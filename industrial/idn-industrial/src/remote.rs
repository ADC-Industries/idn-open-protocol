//! Remote support on the machine (§I.11): screen sessions that stream H.264 up to the
//! Site Manager (which relays it to a browser), input injection, and chat. Roles and
//! notify-or-silent are decided at the Site Manager, which is authenticated; the machine
//! obeys its commands and records what it did to the audit (`audit.remote`).
//!
//! The Site Manager drives it with `remote.*` requests:
//! - `remote.monitors` → `{monitors:[…]}`
//! - `remote.screen.start` `{session, monitor?, fps?, bitrate?, pointer?, notify?, control?, user?}`
//!   → `{width, height}`; the machine opens a screen stream for the session.
//! - `remote.screen.stop` `{session}`
//! - `remote.input` `{session, events:[…]}` → `{clipboard?}` (§I.11.4)
//! - `remote.chat` `{from, text}` (§I.11.5)

use crate::input::{self, Event, Injector};
use crate::node::{Command, IndustrialNode};
use crate::screen::{self, ScreenParams};
use crate::wire;
use idn_common::sync::LockOk as _;
use serde::Deserialize;
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;

/// What the machine shows its operator: the notify banner and the chat popup. The helper
/// running in the user's session sets a real one; the default logs and keeps chat for
/// the local API to hand to the helper when it starts (so a message waits for a login).
pub trait Station: Send + Sync {
    /// A session started or ended: show or hide the banner ("`<user>` is viewing" or
    /// "controlling"), unless `notify` is false (a silent session shows nothing).
    fn session(&self, session: &str, user: &str, controlling: bool, notify: bool, on: bool);
    /// A chat message to the operator.
    fn chat(&self, from: &str, text: &str);
}

/// The default [`Station`]: logs the banner, queues chat for the local API.
#[derive(Default)]
pub struct LogStation {
    pending: Mutex<Vec<ChatMessage>>,
}

/// A chat message kept for the operator (§I.11.5).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChatMessage {
    pub from: String,
    pub text: String,
    pub time_ms: i64,
}

impl LogStation {
    /// The chat messages waiting (and clears them): the helper polls this at login.
    pub fn take_pending(&self) -> Vec<ChatMessage> {
        std::mem::take(&mut self.pending.lock_ok())
    }
}

impl Station for LogStation {
    fn session(&self, _session: &str, user: &str, controlling: bool, notify: bool, on: bool) {
        if notify {
            idn_open_protocol::log::log(&format!(
                "remote: {user} {} {}",
                if controlling {
                    "is controlling"
                } else {
                    "is viewing"
                },
                if on { "(session started)" } else { "ended" }
            ));
        }
    }
    fn chat(&self, from: &str, text: &str) {
        // (Kept for a helper not connected yet: the newest 100, 4000 characters each; it
        // grew with every message, up to 2 MB each; bug review 3.)
        let text: String = text.chars().take(4000).collect();
        idn_open_protocol::log::log(&format!("remote: chat from {from}: {text}"));
        let mut pending = self.pending.lock_ok();
        pending.push(ChatMessage {
            from: from.chars().take(64).collect(),
            text,
            time_ms: wire::now_ms(),
        });
        let extra = pending.len().saturating_sub(100);
        pending.drain(..extra);
    }
}

/// Remote support for one machine.
pub struct Remote {
    node: IndustrialNode,
    enabled: AtomicBool,
    station: Arc<dyn Station>,
    injector: Mutex<Box<dyn Injector>>,
    /// What each session's viewers hold down, let go of when it stops (E3-1).
    held: Mutex<HashMap<String, input::Held>>,
    sessions: Mutex<HashMap<String, SessionHandle>>,
    /// The in-session helper, when one is connected (§I.11.2): it does the capture and
    /// input the service (session 0) can't. `None` means the Node does it directly
    /// (Linux, tests, or before the helper connects).
    worker: Mutex<Option<Arc<crate::worker::WorkerConn>>>,
}

struct SessionHandle {
    stop: Arc<AtomicBool>,
    want_key: Arc<AtomicBool>,
    user: String,
    controlling: bool,
    notify: bool,
    /// For a worker session: where the helper's pictures go (the pump's end), and the
    /// capture to re-request when a new helper connects. `None` for a local capture.
    frame_tx: Option<tokio::sync::mpsc::Sender<crate::worker::ScreenFrame>>,
    params: Option<crate::worker::CaptureParams>,
}

#[derive(Deserialize, Default)]
struct StartBody {
    session: String,
    monitor: Option<u32>,
    fps: Option<u32>,
    bitrate: Option<u64>,
    pointer: Option<bool>,
    #[serde(default)]
    notify: bool,
    #[serde(default)]
    control: bool,
    #[serde(default)]
    user: String,
}

#[derive(Deserialize)]
struct StopBody {
    session: String,
}

#[derive(Deserialize)]
struct InputBody {
    #[serde(default)]
    session: String,
    events: Vec<Event>,
}

#[derive(Deserialize)]
struct ChatBody {
    #[serde(default)]
    from: String,
    text: String,
}

impl Remote {
    pub fn new(node: IndustrialNode, enabled: bool, station: Arc<dyn Station>) -> Arc<Remote> {
        Arc::new(Remote {
            node,
            enabled: AtomicBool::new(enabled),
            station,
            injector: Mutex::new(input::injector()),
            held: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            worker: Mutex::new(None),
        })
    }

    /// A helper connected and became the worker (§I.11.2). Any sessions already running
    /// (a desktop switch relaunched the helper) have their captures re-requested, so the
    /// screen keeps flowing to the same session on the new desktop.
    pub fn attach_worker(&self, conn: Arc<crate::worker::WorkerConn>) {
        *self.worker.lock_ok() = Some(conn.clone());
        for (session, h) in self.sessions.lock_ok().iter() {
            if let Some(p) = h.params {
                conn.resume_capture(session, p);
            }
        }
        idn_open_protocol::log::log("remote: the session helper connected");
    }

    /// The worker disconnected: stop the sessions it was serving (capture goes with it),
    /// but only if this is still the current worker (a newer one may have replaced it).
    pub fn detach_worker(&self, conn: &Arc<crate::worker::WorkerConn>) {
        let mut w = self.worker.lock_ok();
        if w.as_ref().is_some_and(|c| Arc::ptr_eq(c, conn)) {
            *w = None;
            drop(w);
            self.stop_all();
            idn_open_protocol::log::log("remote: the session helper disconnected");
        }
    }

    fn worker(&self) -> Option<Arc<crate::worker::WorkerConn>> {
        self.worker.lock_ok().clone()
    }

    /// Whether an in-session helper is connected (the worker path is in use).
    pub fn has_worker(&self) -> bool {
        self.worker.lock_ok().is_some()
    }

    /// A picture arrived from the helper for `session`: hand it to the session's pump
    /// (dropped if the pump is behind).
    pub fn feed_frame(&self, session: &str, frame: crate::worker::ScreenFrame) {
        let tx = self
            .sessions
            .lock_ok()
            .get(session)
            .and_then(|h| h.frame_tx.clone());
        if let Some(tx) = tx {
            let _ = tx.try_send(frame);
        }
    }

    /// Turns screen and input sessions on or off (a settings reload); stops running
    /// sessions when turned off.
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
        if !on {
            self.stop_all();
        }
    }

    /// Handles a `remote.*` command; `None` if it isn't one.
    pub async fn handle(self: &Arc<Self>, c: &Command) -> Option<(u8, Vec<u8>)> {
        let r = match c.kind.as_str() {
            "remote.monitors" => Ok(serde_json::json!({ "monitors": monitors_json() })),
            "remote.screen.start" => self.start(&c.payload).await,
            "remote.screen.stop" => self.stop(&c.payload),
            "remote.input" => self.input(&c.payload).await,
            "remote.chat" => self.chat(&c.payload),
            _ => return None,
        };
        Some(match r {
            Ok(v) => (wire::ST_OK, serde_json::to_vec(&v).unwrap_or_default()),
            Err(e) if e.kind() == io::ErrorKind::Unsupported => {
                (wire::ST_REFUSED, e.to_string().into_bytes())
            }
            Err(e) => (wire::ST_ERROR, e.to_string().into_bytes()),
        })
    }

    fn check_enabled(&self) -> io::Result<()> {
        if self.enabled.load(Ordering::Relaxed) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "remote support is off on this machine",
            ))
        }
    }

    async fn start(self: &Arc<Self>, payload: &[u8]) -> io::Result<serde_json::Value> {
        self.check_enabled()?;
        let b: StartBody =
            serde_json::from_slice(payload).map_err(idn_common::err::invalid_data)?;
        if b.session.is_empty() {
            return Err(idn_common::err::invalid_data("no session id"));
        }
        let session = b.session.clone();
        let fps = b.fps.unwrap_or(15).clamp(1, 60);
        let bitrate = b.bitrate.unwrap_or(4_000_000);
        let pointer = b.pointer.unwrap_or(true);
        let stop = Arc::new(AtomicBool::new(false));
        let want_key = Arc::new(AtomicBool::new(false));
        // The pictures reach the pump through this channel, from the helper (the worker)
        // or from a local capture thread. A bounded channel drops old pictures on a slow
        // link rather than piling them up.
        let (tx, rx) = tokio::sync::mpsc::channel::<crate::worker::ScreenFrame>(4);
        // For a worker session, the pump's end of the channel is kept so the helper's
        // pictures (routed through feed_frame) reach it, and the params so the capture can
        // resume on reconnect. A local capture hands `tx` to its thread instead.
        let (mut frame_tx, mut params) = (None, None);
        let (w, h) = match self.worker() {
            Some(worker) => {
                let p = crate::worker::CaptureParams {
                    monitor: b.monitor,
                    fps,
                    bitrate,
                    pointer,
                };
                let size = worker.start_capture(&session, p).await?;
                frame_tx = Some(tx);
                params = Some(p);
                size
            }
            None => {
                let sp = ScreenParams {
                    monitor: b.monitor,
                    fps,
                    bitrate,
                    pointer,
                };
                self.start_local_capture(sp, stop.clone(), want_key.clone(), tx)?
            }
        };
        let me = self.clone();
        let user = if b.user.is_empty() {
            "someone".to_owned()
        } else {
            b.user.clone()
        };
        let notify = b.notify;
        let controlling = b.control;
        let writer_stop = stop.clone();
        self.sessions.lock_ok().insert(
            session.clone(),
            SessionHandle {
                stop,
                want_key,
                user: user.clone(),
                controlling,
                notify,
                frame_tx: frame_tx.take(),
                params: params.take(),
            },
        );
        self.station
            .session(&session, &user, controlling, notify, true);
        self.audit("start", &session, &user, controlling, notify);
        // The pump last: one that ends at once (the link just dropped) then finds the
        // session to remove and the banner to turn off.
        let (session2, user2) = (session.clone(), user.clone());
        crate::node::runtime().spawn(async move {
            me.pump(session2, user2, controlling, notify, rx, writer_stop)
                .await;
        });
        Ok(serde_json::json!({ "width": w, "height": h }))
    }

    /// Captures on a local thread (no helper): the pictures go to `tx` as [`crate::worker::ScreenFrame`].
    fn start_local_capture(
        &self,
        params: ScreenParams,
        stop: Arc<AtomicBool>,
        want_key: Arc<AtomicBool>,
        tx: tokio::sync::mpsc::Sender<crate::worker::ScreenFrame>,
    ) -> io::Result<(u32, u32)> {
        let (size_tx, size_rx) = std::sync::mpsc::channel::<(u32, u32)>();
        std::thread::Builder::new()
            .name("idn-remote-capture".into())
            .spawn(move || {
                let mut cap = screen::open(&params);
                let _ = size_tx.send(cap.size());
                while !stop.load(Ordering::Relaxed) {
                    if want_key.swap(false, Ordering::Relaxed) {
                        cap.want_keyframe();
                    }
                    match cap.next() {
                        Ok(pic) => {
                            let f = crate::worker::ScreenFrame {
                                keyframe: pic.keyframe,
                                pts_ms: pic.pts_ms,
                                data: pic.data,
                            };
                            if tx.blocking_send(f).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            idn_open_protocol::log::log(&format!("remote: capture ended: {e}"));
                            break;
                        }
                    }
                }
            })?;
        size_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| io::Error::other("the screen capture didn't open in time"))
    }

    /// Opens the session's IDN stream and writes each picture up it until the session
    /// ends (§I.11.3), then cleans up (banner off, audit, and the worker's capture).
    async fn pump(
        self: Arc<Self>,
        session: String,
        user: String,
        controlling: bool,
        notify: bool,
        mut rx: tokio::sync::mpsc::Receiver<crate::worker::ScreenFrame>,
        writer_stop: Arc<AtomicBool>,
    ) {
        let opened = match self.node.open_screen(&session).await {
            Ok(x) => Some(x),
            Err(e) => {
                idn_open_protocol::log::log(&format!("remote: opening the screen stream: {e}"));
                None
            }
        };
        if let Some((mut s, _r)) = opened {
            while let Some(f) = rx.recv().await {
                if writer_stop.load(Ordering::Relaxed) {
                    break;
                }
                let mut buf = Vec::with_capacity(13 + f.data.len());
                buf.push(f.keyframe as u8);
                buf.extend_from_slice(&f.pts_ms.to_le_bytes());
                buf.extend_from_slice(&(f.data.len() as u32).to_le_bytes());
                buf.extend_from_slice(&f.data);
                if s.write_all(&buf).await.is_err() || s.flush().await.is_err() {
                    break;
                }
            }
            let _ = s.shutdown().await;
        }
        writer_stop.store(true, Ordering::Relaxed);
        if let Some(w) = self.worker() {
            w.stop_capture(&session);
        }
        self.sessions.lock_ok().remove(&session);
        self.station
            .session(&session, &user, controlling, notify, false);
        self.audit("end", &session, &user, controlling, notify);
    }

    fn stop(&self, payload: &[u8]) -> io::Result<serde_json::Value> {
        let b: StopBody = serde_json::from_slice(payload).map_err(idn_common::err::invalid_data)?;
        if let Some(h) = self.sessions.lock_ok().get(&b.session) {
            h.stop.store(true, Ordering::Relaxed);
        }
        // Drops the worker's route for this session, so its pump sees the channel close
        // and ends (the worker stops capturing).
        if let Some(w) = self.worker() {
            w.stop_capture(&b.session);
        }
        if let Some(mut h) = self.held.lock_ok().remove(&b.session) {
            let mut inj = self.injector.lock_ok();
            for e in h.release() {
                let _ = inj.inject(&e);
            }
        }
        Ok(serde_json::json!({ "stopped": true }))
    }

    async fn input(&self, payload: &[u8]) -> io::Result<serde_json::Value> {
        self.check_enabled()?;
        let b: InputBody =
            serde_json::from_slice(payload).map_err(idn_common::err::invalid_data)?;
        // Only for a session this machine is showing (its banner on, its start audited).
        // A new viewer's first input often follows a join; ask for a keyframe so a late
        // decoder can start. (Harmless if one is already due.)
        match self.sessions.lock_ok().get(&b.session) {
            Some(h) if h.controlling => h.want_key.store(true, Ordering::Relaxed),
            Some(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "this screen session is view-only",
                ));
            }
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "no such screen session",
                ));
            }
        }
        // The helper injects on the interactive desktop when one is connected; otherwise
        // the Node does (Linux, tests).
        if let Some(worker) = self.worker() {
            let events = serde_json::to_value(&b.events).unwrap_or_default();
            let clip = worker.input(&b.session, &events).await;
            return Ok(match clip {
                Some(t) => serde_json::json!({ "clipboard": t }),
                None => serde_json::json!({}),
            });
        }
        let mut clipboard = None;
        let mut inj = self.injector.lock_ok();
        let mut held = self.held.lock_ok();
        let h = held.entry(b.session.clone()).or_default();
        for e in &b.events {
            match inj.inject(e) {
                Ok(Some(text)) => {
                    h.note(e);
                    clipboard = Some(text);
                }
                Ok(None) => h.note(e),
                Err(e) => return Err(e),
            }
        }
        Ok(match clipboard {
            Some(t) => serde_json::json!({ "clipboard": t }),
            None => serde_json::json!({}),
        })
    }

    fn chat(&self, payload: &[u8]) -> io::Result<serde_json::Value> {
        let b: ChatBody = serde_json::from_slice(payload).map_err(idn_common::err::invalid_data)?;
        let from = if b.from.is_empty() {
            "support"
        } else {
            &b.from
        };
        // The helper shows it on the station when connected; otherwise it is queued for
        // the helper to show at the next login.
        match self.worker() {
            Some(worker) => worker.chat(from, &b.text),
            None => self.station.chat(from, &b.text),
        }
        Ok(serde_json::json!({ "shown": true }))
    }

    /// The operator's reply from the station's chat window (§I.11.5): sent up to the Site
    /// Manager, which audits it and publishes it on `idn/site/events`.
    pub fn chat_reply(&self, text: &str) {
        let v = serde_json::json!({ "user": "station", "text": text, "to_station": false });
        let node = self.node.clone();
        crate::node::runtime().spawn(async move {
            let _ = tokio::task::spawn_blocking(move || {
                node.send("audit.chat", serde_json::to_vec(&v).unwrap_or_default())
            })
            .await;
        });
    }

    /// Records a session event to the audit (the Site Manager stores `audit.*`).
    fn audit(&self, event: &str, session: &str, user: &str, controlling: bool, notify: bool) {
        let v = serde_json::json!({
            "event": event,
            "session": session,
            "user": user,
            "control": controlling,
            "notify": notify,
        });
        let node = self.node.clone();
        crate::node::runtime().spawn(async move {
            let _ = tokio::task::spawn_blocking(move || {
                node.send("audit.remote", serde_json::to_vec(&v).unwrap_or_default())
            })
            .await;
        });
    }

    /// Ends every session (on shutdown or a settings reload).
    pub fn stop_all(&self) {
        for h in self.sessions.lock_ok().values() {
            h.stop.store(true, Ordering::Relaxed);
        }
    }

    /// The sessions now running (for the local API / panel).
    pub fn sessions(&self) -> Vec<serde_json::Value> {
        self.sessions
            .lock_ok()
            .iter()
            .map(|(id, h)| {
                serde_json::json!({
                    "session": id,
                    "user": h.user,
                    "control": h.controlling,
                    "notify": h.notify,
                })
            })
            .collect()
    }
}

fn monitors_json() -> Vec<serde_json::Value> {
    screen::monitors()
        .into_iter()
        .map(|m| {
            serde_json::json!({
                "index": m.index,
                "name": m.name,
                "width": m.width,
                "height": m.height,
                "primary": m.primary,
            })
        })
        .collect()
}
