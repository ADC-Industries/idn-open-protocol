//! The session worker channel (§I.11.2): on Windows the Node runs as a service in session
//! 0, where it cannot capture or drive the user's screen, so the capture and input happen
//! in the helper the service launches onto the interactive desktop (`src/session.rs`).
//! The helper connects back to the Node's local API over a WebSocket and becomes the
//! Node's "worker": the Node forwards each screen session's start/stop, input and chat to
//! it, and relays the pictures it sends up the IDN link to the Site Manager.
//!
//! When no worker is connected (Linux, or before the helper starts) the Node captures and
//! injects directly ([`crate::remote`]'s own path), so the same code is testable without
//! a helper. The helper side ([`run_worker`]) uses the synthetic capture where there is no
//! display, so the whole relay is exercised in tests.
//!
//! Protocol (loopback only, the local API token): the Node sends JSON commands as text —
//! `{"op":"start",session,monitor,fps,bitrate,pointer}`, `{"op":"stop",session}`,
//! `{"op":"input",session,events}`, `{"op":"chat",from,text}`. The helper sends JSON back
//! — `{"op":"size",session,width,height}`, `{"op":"clipboard",session,text}` — and each
//! picture as one binary message: `sidlen (u8) | session | keyframe (u8) | pts_ms (i64 LE)
//! | H.264`.

use crate::input::{self, Event};
use crate::screen::{self, ScreenParams};
use idn_common::sync::LockOk as _;
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// One encoded picture from the worker, bound for a session's IDN stream.
pub struct ScreenFrame {
    pub keyframe: bool,
    pub pts_ms: i64,
    pub data: Vec<u8>,
}

/// How a session wants its screen captured, kept so the capture can be re-requested when a
/// new helper connects (a desktop switch relaunches the helper, §I.11.2).
#[derive(Debug, Clone, Copy)]
pub struct CaptureParams {
    pub monitor: Option<u32>,
    pub fps: u32,
    pub bitrate: u64,
    pub pointer: bool,
}

/// The Node's handle to a connected helper. Commands go out through `out` (written on the
/// socket by the serve loop); the helper's size and clipboard answers wake the waiters.
/// Pictures are routed by the session, through [`crate::remote::Remote`], not here, so they
/// keep flowing to the same session when the helper reconnects.
pub struct WorkerConn {
    out: mpsc::UnboundedSender<axum::extract::ws::Message>,
    sizes: Mutex<HashMap<String, oneshot::Sender<(u32, u32)>>>,
    clips: Mutex<HashMap<String, oneshot::Sender<String>>>,
}

impl WorkerConn {
    fn send(&self, v: serde_json::Value) {
        let _ = self
            .out
            .send(axum::extract::ws::Message::Text(v.to_string().into()));
    }

    fn send_start(&self, session: &str, p: CaptureParams) {
        self.send(serde_json::json!({
            "op": "start", "session": session, "monitor": p.monitor,
            "fps": p.fps, "bitrate": p.bitrate, "pointer": p.pointer,
        }));
    }

    /// Asks the helper to capture `session` and returns the picture size once it answers.
    pub async fn start_capture(&self, session: &str, p: CaptureParams) -> io::Result<(u32, u32)> {
        let (tx, rx) = oneshot::channel();
        self.sizes.lock_ok().insert(session.to_owned(), tx);
        self.send_start(session, p);
        match tokio::time::timeout(Duration::from_secs(8), rx).await {
            Ok(Ok(size)) => Ok(size),
            _ => {
                self.sizes.lock_ok().remove(session);
                Err(io::Error::other(
                    "the helper didn't start the capture in time",
                ))
            }
        }
    }

    /// Re-requests a capture on a freshly connected helper (its size answer is ignored, the
    /// session already knows it).
    pub fn resume_capture(&self, session: &str, p: CaptureParams) {
        self.send_start(session, p);
    }

    /// Stops a capture.
    pub fn stop_capture(&self, session: &str) {
        self.sizes.lock_ok().remove(session);
        self.clips.lock_ok().remove(session);
        self.send(serde_json::json!({ "op": "stop", "session": session }));
    }

    /// Forwards input to the helper; returns the clipboard text if a get was asked for.
    pub async fn input(&self, session: &str, events: &serde_json::Value) -> Option<String> {
        let wants_clip = events.as_array().is_some_and(|a| {
            a.iter()
                .any(|e| e.get("t").and_then(|t| t.as_str()) == Some("clipboardget"))
        });
        let rx = wants_clip.then(|| {
            let (tx, rx) = oneshot::channel();
            self.clips.lock_ok().insert(session.to_owned(), tx);
            rx
        });
        self.send(serde_json::json!({ "op": "input", "session": session, "events": events }));
        match rx {
            Some(rx) => tokio::time::timeout(Duration::from_secs(5), rx)
                .await
                .ok()
                .and_then(Result::ok),
            None => None,
        }
    }

    /// Asks the helper to show a chat message on the station.
    pub fn chat(&self, from: &str, text: &str) {
        self.send(serde_json::json!({ "op": "chat", "from": from, "text": text }));
    }
}

/// Serves a helper's worker connection: registers it on `remote`, writes the Node's
/// commands to it, and dispatches its pictures and answers until it disconnects.
pub async fn serve(remote: Arc<crate::remote::Remote>, socket: axum::extract::ws::WebSocket) {
    use futures::{SinkExt, StreamExt};
    let (out, mut out_rx) = mpsc::unbounded_channel();
    let conn = Arc::new(WorkerConn {
        out,
        sizes: Mutex::new(HashMap::new()),
        clips: Mutex::new(HashMap::new()),
    });
    remote.attach_worker(conn.clone());
    let (mut sink, mut stream) = socket.split();
    let writer = tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            if sink.send(m).await.is_err() {
                break;
            }
        }
    });
    while let Some(Ok(msg)) = stream.next().await {
        use axum::extract::ws::Message;
        match msg {
            Message::Binary(b) => route_frame(&remote, &b),
            Message::Text(t) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                    let session = v.get("session").and_then(|s| s.as_str()).unwrap_or("");
                    match v.get("op").and_then(|o| o.as_str()) {
                        Some("size") => {
                            let w = v.get("width").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
                            let h = v.get("height").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
                            if let Some(tx) = conn.sizes.lock_ok().remove(session) {
                                let _ = tx.send((w, h));
                            }
                        }
                        Some("clipboard") => {
                            let text = v
                                .get("text")
                                .and_then(|t| t.as_str())
                                .unwrap_or("")
                                .to_owned();
                            if let Some(tx) = conn.clips.lock_ok().remove(session) {
                                let _ = tx.send(text);
                            }
                        }
                        // The operator typed a reply in the chat window.
                        Some("chatreply") => {
                            let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("");
                            remote.chat_reply(text);
                        }
                        _ => {}
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    writer.abort();
    remote.detach_worker(&conn);
}

/// Parses a binary picture `sidlen | session | keyframe | pts_ms | H.264` and hands it to
/// the session's pump through the Node (dropped if the pump is behind, so a slow link
/// never blocks the helper).
fn route_frame(remote: &crate::remote::Remote, b: &[u8]) {
    if b.is_empty() {
        return;
    }
    let sl = b[0] as usize;
    if b.len() < 1 + sl + 9 {
        return;
    }
    let session = String::from_utf8_lossy(&b[1..1 + sl]).into_owned();
    let p = 1 + sl;
    let keyframe = b[p] != 0;
    let pts_ms = i64::from_le_bytes(b[p + 1..p + 9].try_into().unwrap());
    let data = b[p + 9..].to_vec();
    remote.feed_frame(
        &session,
        ScreenFrame {
            keyframe,
            pts_ms,
            data,
        },
    );
}

// ---------------------------------------------------------------------------------------
// The helper side.

/// Runs the helper's worker loop: connects to the Node's local API and serves screen
/// captures, input and chat on the interactive desktop until `stop` is set. Reconnects if
/// the link drops. `base` is like `http://127.0.0.1:29208`; `token` the local API token.
pub async fn run_worker(base: &str, token: &str, stop: Arc<AtomicBool>) {
    let url = base
        .replacen("http://", "ws://", 1)
        .replacen("https://", "wss://", 1)
        + "/remote/worker";
    while !stop.load(Ordering::Relaxed) {
        match connect(&url, token).await {
            Ok(ws) => session_loop(ws, &stop).await,
            Err(e) => idn_open_protocol::log::log(&format!("helper: connecting to the Node: {e}")),
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(url: &str, token: &str) -> io::Result<Ws> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = url.into_client_request().map_err(idn_common::err::other)?;
    req.headers_mut().insert(
        "authorization",
        format!("Bearer {token}")
            .parse()
            .map_err(|_| io::Error::other("bad token"))?,
    );
    let (ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .map_err(idn_common::err::other)?;
    Ok(ws)
}

/// One connection: serves commands until it drops.
async fn session_loop(ws: Ws, stop: &AtomicBool) {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let (mut sink, mut stream) = ws.split();
    // Pictures and answers from the captures go out through this channel.
    let (out, mut out_rx) = mpsc::unbounded_channel::<Message>();
    let mut captures: HashMap<String, Capture> = HashMap::new();
    let injector = Arc::new(Mutex::new(input::injector()));
    // What each session's viewers hold down, let go of when it stops (bug round E, E3-1).
    let mut held: HashMap<String, input::Held> = HashMap::new();
    // The chat window, opened on the first message. Replies go back as `chatreply`.
    let mut chat: Option<crate::chat::ChatWindow> = None;
    loop {
        tokio::select! {
            m = out_rx.recv() => match m {
                Some(m) => { if sink.send(m).await.is_err() { break; } }
                None => break,
            },
            msg = stream.next() => match msg {
                Some(Ok(Message::Text(t))) => {
                    handle_command(&t, &mut captures, &injector, &out, &mut chat, &mut held);
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(_)) => break,
            },
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
    }
    for (_, c) in captures.drain() {
        c.stop();
    }
    // (The Node gone: everything held let go.)
    let mut inj = injector.lock_ok();
    for (_, mut h) in held.drain() {
        for e in h.release() {
            let _ = inj.inject(&e);
        }
    }
}

/// A running capture on the helper: a thread encoding, feeding the out channel.
struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Capture {
    fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn handle_command(
    text: &str,
    captures: &mut HashMap<String, Capture>,
    injector: &Arc<Mutex<Box<dyn input::Injector>>>,
    out: &mpsc::UnboundedSender<tokio_tungstenite::tungstenite::Message>,
    chat: &mut Option<crate::chat::ChatWindow>,
    held: &mut HashMap<String, input::Held>,
) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    let op = v.get("op").and_then(|o| o.as_str()).unwrap_or("");
    let session = v
        .get("session")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_owned();
    match op {
        "start" => {
            if captures.contains_key(&session) {
                return;
            }
            let params = ScreenParams {
                monitor: v.get("monitor").and_then(|m| m.as_u64()).map(|m| m as u32),
                fps: v
                    .get("fps")
                    .and_then(|f| f.as_u64())
                    .unwrap_or(15)
                    .clamp(1, 60) as u32,
                bitrate: v
                    .get("bitrate")
                    .and_then(|b| b.as_u64())
                    .unwrap_or(4_000_000),
                pointer: v.get("pointer").and_then(|p| p.as_bool()).unwrap_or(true),
            };
            captures.insert(session.clone(), start_capture(session, params, out.clone()));
        }
        "stop" => {
            if let Some(c) = captures.remove(&session) {
                c.stop();
            }
            if let Some(mut h) = held.remove(&session) {
                let mut inj = injector.lock_ok();
                for e in h.release() {
                    let _ = inj.inject(&e);
                }
            }
        }
        "input" => {
            let mut clip = None;
            if let Some(events) = v.get("events").and_then(|e| e.as_array()) {
                let mut inj = injector.lock_ok();
                let h = held.entry(session.clone()).or_default();
                for e in events {
                    if let Ok(ev) = serde_json::from_value::<Event>(e.clone()) {
                        match inj.inject(&ev) {
                            Ok(Some(text)) => {
                                h.note(&ev);
                                clip = Some(text);
                            }
                            Ok(None) => h.note(&ev),
                            Err(_) => {}
                        }
                    }
                }
            }
            if let Some(text) = clip {
                let _ = out.send(tungstenite_text(serde_json::json!({
                    "op": "clipboard", "session": session, "text": text,
                })));
            }
        }
        "chat" => {
            let from = v.get("from").and_then(|f| f.as_str()).unwrap_or("support");
            let txt = v.get("text").and_then(|t| t.as_str()).unwrap_or("");
            // The popup is opened on the first message; the operator's replies go back as
            // `chatreply`, which the Node relays to the Site Manager.
            let window = chat.get_or_insert_with(|| {
                let out = out.clone();
                crate::chat::ChatWindow::open(move |reply| {
                    let _ = out.send(tungstenite_text(serde_json::json!({
                        "op": "chatreply", "text": reply,
                    })));
                })
            });
            window.show(from, txt);
        }
        _ => {}
    }
}

fn tungstenite_text(v: serde_json::Value) -> tokio_tungstenite::tungstenite::Message {
    tokio_tungstenite::tungstenite::Message::Text(v.to_string().into())
}

/// Opens a capture on a thread; sends its size, then each picture as a binary message
/// (`sidlen | session | keyframe | pts_ms | H.264`).
fn start_capture(
    session: String,
    params: ScreenParams,
    out: mpsc::UnboundedSender<tokio_tungstenite::tungstenite::Message>,
) -> Capture {
    use tokio_tungstenite::tungstenite::Message;
    let stop = Arc::new(AtomicBool::new(false));
    let cap_stop = stop.clone();
    let thread = std::thread::Builder::new()
        .name("idn-helper-capture".into())
        .spawn(move || {
            let mut cap = screen::open(&params);
            let (w, h) = cap.size();
            let _ = out.send(tungstenite_text(serde_json::json!({
                "op": "size", "session": session, "width": w, "height": h,
            })));
            let sid = session.as_bytes();
            while !cap_stop.load(Ordering::Relaxed) {
                match cap.next() {
                    Ok(pic) => {
                        let mut buf = Vec::with_capacity(10 + sid.len() + pic.data.len());
                        buf.push(sid.len() as u8);
                        buf.extend_from_slice(sid);
                        buf.push(pic.keyframe as u8);
                        buf.extend_from_slice(&pic.pts_ms.to_le_bytes());
                        buf.extend_from_slice(&pic.data);
                        if out.send(Message::Binary(buf.into())).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        idn_open_protocol::log::log(&format!("helper: capture ended: {e}"));
                        break;
                    }
                }
            }
        })
        .ok();
    Capture { stop, thread }
}
