//! A connection's session, whichever transport carries it (the control WebSocket, §V.8,
//! or IDN's link, §V.14): hello, commands and sound until it closes or is silent for the
//! site's `idle_timeout_s`. Both carry the same JSON and the same binary sound messages.

use crate::Voice;
use crate::hub::{Out, Target, Who};
use crate::model::UserSettings;
use axum::extract::ws::{Message, WebSocket};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::net::IpAddr;
use std::time::Duration;

/// JSON messages waiting for one connection past which it is closed rather than any
/// dropped (a client that stopped reading; a handset then resumes, §V.13.6).
const CONTROL_BEHIND: usize = 10_000;

/// How long a closing connection waits for its last messages (a `bye`) to go.
const LINGER: Duration = Duration::from_secs(2);

/// How often a write that waits checks the JSON queued behind it.
const CHECK_EVERY: Duration = Duration::from_millis(250);

/// A connection's sending half.
pub(crate) enum Writer {
    Ws(SplitSink<WebSocket, Message>),
    /// The stream, and its connection (closed at once when the client stops reading).
    Link(quinn::SendStream, quinn::Connection),
}

/// Why a connection's writer gave up on its client.
#[derive(Debug, Clone, Copy)]
enum Stall {
    /// A write took nothing for the stall time (the session's idle timeout).
    NotReading,
    /// More than [`CONTROL_BEHIND`] JSON messages queued behind a write that waits.
    Behind,
}

impl Stall {
    fn why(self) -> &'static str {
        match self {
            Stall::NotReading => "not reading",
            Stall::Behind => "too far behind",
        }
    }
}

/// A connection's receiving half.
pub(crate) enum Reader {
    Ws(SplitStream<WebSocket>),
    Link(quinn::RecvStream, quinn::Connection),
}

/// What a client sent.
pub(crate) enum In {
    Json(String),
    /// A sound message: a kind byte (1), then the Opus packet.
    Sound(Vec<u8>),
    Close,
    Other,
}

impl Writer {
    /// A JSON message; `false`: the session ends (the connection gone, or a message
    /// longer than clients take, 4 MiB: never sent, logged).
    async fn json(&mut self, v: &Value) -> bool {
        let text = v.to_string();
        let too_long = |n: usize| {
            idn_open_protocol::log::log(&format!(
                "voice: a {} message of {n} bytes not sent (at most {}): the session ends",
                v["t"],
                crate::link::MAX_FROM_SERVER
            ));
            false
        };
        match self {
            Writer::Ws(_) if text.len() > crate::link::MAX_FROM_SERVER => too_long(text.len()),
            Writer::Ws(w) => w.send(Message::Text(text.into())).await.is_ok(),
            Writer::Link(s, _) => match crate::link::frame(crate::link::KIND_JSON, text.as_bytes())
            {
                Ok(f) => s.write_all(&f).await.is_ok(),
                Err(_) => too_long(text.len() + 1),
            },
        }
    }

    async fn sound(&mut self, b: Vec<u8>) -> bool {
        match self {
            Writer::Ws(w) => w.send(Message::Binary(b.into())).await.is_ok(),
            Writer::Link(s, _) => match crate::link::frame(crate::link::KIND_SOUND, &b) {
                Ok(f) => s.write_all(&f).await.is_ok(),
                Err(_) => false,
            },
        }
    }

    /// Ends the sending side; it waits (a while) for what was written to arrive.
    pub(crate) async fn close(&mut self) {
        match self {
            Writer::Ws(w) => {
                let _ = tokio::time::timeout(LINGER, w.close()).await;
            }
            Writer::Link(s, _) => {
                let _ = s.finish();
                let _ = tokio::time::timeout(LINGER, s.stopped()).await;
            }
        }
    }

    /// Ends the connection at once, for a client that stopped reading: a link's closed
    /// with the reason (a `bye` behind what it hasn't read would never reach it); a
    /// WebSocket's let go with the session.
    fn abandon(&mut self, why: &str) {
        if let Writer::Link(_, c) = self {
            c.close(crate::link::DONE.into(), why.as_bytes());
        }
    }
}

/// A write raced against its client: `Err` once it has waited `stall` (the client takes
/// nothing) or the JSON queued behind it passes [`CONTROL_BEHIND`] meanwhile. A client
/// that stopped reading held the writer in its write for good, its queue growing, and
/// the check for that never ran.
async fn watched(
    write: impl std::future::Future<Output = bool>,
    rx: &tokio::sync::mpsc::UnboundedReceiver<Out>,
    stall: Duration,
) -> Result<bool, Stall> {
    use futures_util::FutureExt as _;
    let mut write = std::pin::pin!(write);
    // (Most writes go at once: no timer for those.)
    if let Some(ok) = write.as_mut().now_or_never() {
        return Ok(ok);
    }
    let since = tokio::time::Instant::now();
    loop {
        if let Ok(ok) = tokio::time::timeout(CHECK_EVERY, write.as_mut()).await {
            return Ok(ok);
        }
        if rx.len() > CONTROL_BEHIND {
            return Err(Stall::Behind);
        }
        if since.elapsed() >= stall {
            return Err(Stall::NotReading);
        }
    }
}

/// A connection's writer: replies and events first, each time round; between them one
/// sound frame at a time, oldest first (a slow link gets it late, not lost, within its
/// window; a long backlog never holds replies up). `Ok` once the session is over (a
/// `bye` sent, the connection gone); `Err` for a client that stopped reading.
async fn write_out(
    wr: &mut Writer,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Out>,
    sound: &crate::hub::FrameQueue,
    stall: Duration,
) -> Result<(), Stall> {
    loop {
        // A client that stopped reading: closed, nothing dropped (a handset resumes).
        // Its `bye` only if that goes within a moment (else the link is closed with the
        // reason: a `bye` behind what it hasn't read would never reach it).
        if rx.len() > CONTROL_BEHIND {
            watched(
                wr.json(&json!({"t": "bye", "why": "too far behind"})),
                rx,
                Duration::ZERO,
            )
            .await?;
            return Ok(());
        }
        let o = match rx.try_recv() {
            Ok(o) => Some(o),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => None,
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                if let Some(f) = sound.take() {
                    if !watched(wr.sound(f), rx, stall).await? {
                        return Ok(());
                    }
                    continue;
                }
                tokio::select! {
                    biased;
                    o = rx.recv() => o,
                    _ = sound.ready.notified() => continue,
                }
            }
        };
        let Some(o) = o else { return Ok(()) };
        let sent = match o {
            Out::Json(v) => watched(wr.json(&v), rx, stall).await?,
            Out::Frame(b) => watched(wr.sound(b), rx, stall).await?,
            Out::Close(why) => {
                watched(wr.json(&json!({"t": "bye", "why": why})), rx, stall).await?;
                return Ok(());
            }
        };
        if !sent {
            return Ok(());
        }
    }
}

impl Reader {
    /// The next message; `None` once the connection has ended or broken.
    async fn recv(&mut self) -> Option<In> {
        match self {
            Reader::Ws(r) => Some(match r.next().await?.ok()? {
                Message::Text(t) => In::Json(t.as_str().to_owned()),
                Message::Binary(b) => In::Sound(b.to_vec()),
                Message::Close(_) => In::Close,
                _ => In::Other,
            }),
            Reader::Link(r, conn) => match crate::link::read_frame(r).await {
                Ok(Some((crate::link::KIND_JSON, p))) => {
                    Some(String::from_utf8(p).map_or(In::Other, In::Json))
                }
                Ok(Some((crate::link::KIND_SOUND, p))) => Some(In::Sound(p)),
                // (A kind this server doesn't know: skipped, for later ones.)
                Ok(Some(_)) => Some(In::Other),
                Ok(None) => None,
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::InvalidData {
                        conn.close(crate::link::PROTOCOL.into(), e.to_string().as_bytes());
                    }
                    None
                }
            },
        }
    }
}

/// A connection's life: hello, commands and sound until it closes or is silent for the
/// site's `idle_timeout_s` (30 s). A handset's (§V.13) may come back to its session with
/// `resume`, a token from a `hello`, and `have`, its last `seq` from each talker; its
/// session waits for that when the connection goes.
pub(crate) async fn run(
    voice: Voice,
    (mut rd, mut wr): (Reader, Writer),
    who: Who,
    peer: IpAddr,
    resume: Option<String>,
    have: Option<String>,
) {
    // JSON is never dropped (it was, past 256 queued, and a reader then waited for good);
    // sound has its own queue (hub::FrameQueue).
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Out>();
    let (feed, program) = (who.feed, who.program);
    let tag = who.handset.clone();
    let handset = tag.as_ref().map(|h| h.id.clone());
    let name = who.name.clone();
    let hub = voice.hub.clone();
    if !who.user && who.feed.is_none() {
        hub.host.audit(
            &who.name,
            "voice.guest",
            json!({"address": peer.to_string()}),
        );
    }
    let idle = Duration::from_secs(hub.settings().idle_timeout_s.max(20) as u64);
    // A handset back with its token: its session as it was, its waiting sound sent first.
    let have = have.map(|h| crate::hub::frames::parse_have(&h));
    let resumed = match (&tag, resume.as_deref()) {
        (Some(h), Some(t)) => hub.handset_resume(h, t, tx.clone(), have.as_ref()),
        _ => None,
    };
    let (sid, frames, generation) = match resumed {
        Some(r) => r,
        None => {
            // (None: a handset revoked or enrolled again since its credential was checked.)
            let Some((sid, frames)) = hub.connect(who, tx.clone()) else {
                let bye = json!({"t": "bye", "why": "not an enrolled handset"});
                let _ = tokio::time::timeout(LINGER, wr.json(&bye)).await;
                wr.close().await;
                return;
            };
            // A handset afresh: an earlier session of it taken over (its channels
            // joined again), its driver's channels joined (§V.13).
            if handset.is_some() {
                hub.handset_connected(sid);
            }
            (sid, frames, 0)
        }
    };
    if let Some(h) = &handset {
        hub.handset_seen(h);
    }
    if let Some(c) = feed {
        // A feed that can't join or talk (the channel full) ends its connection, so the
        // program connects again later: it stayed connected and silent for good (bug round
        // E, E5-10).
        let joined = hub.join(sid, c, None).and_then(|()| {
            // An IEM feed talks all the time; a talk source starts its own; voice-out
            // listens.
            if program == crate::hub::Program::Feed {
                hub.talk_start(sid, Target::Channel(c)).map(|_| ())
            } else {
                Ok(())
            }
        });
        if let Err(e) = joined {
            hub.host.audit(
                "voice",
                "voice.feed_refused",
                json!({"channel": c, "why": e.to_string()}),
            );
            hub.disconnect(sid);
            return;
        }
    }
    let sound = frames.clone();
    let writer_name = name.clone();
    // A write that takes nothing for the idle timeout: the client is as gone as one that
    // sends nothing for it (the session ends; a handset resumes).
    let stall = idle;
    let mut writer = tokio::spawn(async move {
        match write_out(&mut wr, &mut rx, &sound, stall).await {
            Ok(()) => wr.close().await,
            Err(s) => {
                idn_open_protocol::log::log(&format!(
                    "voice: {writer_name}: {}; its connection closed",
                    s.why()
                ));
                wr.abandon(s.why());
            }
        }
    });
    // On the link, the site's `link` turned off ends the session: its client goes to the
    // WebSocket (a handset resumes there).
    let on_link = matches!(rd, Reader::Link(..));
    let link_off = async {
        if on_link {
            hub.link_off().await
        } else {
            std::future::pending::<()>().await
        }
    };
    tokio::pin!(link_off);
    let mut bye = false;
    loop {
        // (The writer ending, the server closed the session — a revoked handset, one that
        // connected again — or the connection went: this ends too.)
        let next = tokio::select! {
            m = tokio::time::timeout(idle, rd.recv()) => m,
            _ = &mut writer => break,
            _ = &mut link_off => {
                let _ = tx.send(Out::Close("QUIC is off on this site".into()));
                let _ = (&mut writer).await;
                break;
            }
        };
        let m = match next {
            Ok(Some(m)) => m,
            _ => break,
        };
        match m {
            In::Json(t) => {
                let Ok(v) = serde_json::from_str::<Value>(&t) else {
                    continue;
                };
                // Ended by the server (revoked, taken over), or resumed on another
                // connection: this one acts for nobody now.
                if !hub.is_current(sid, generation) {
                    break;
                }
                // Signing off on purpose: a handset's session ends now, not waiting.
                if v["t"] == "bye" {
                    bye = true;
                    break;
                }
                if let Some(reply) = command(&voice, sid, &v, peer).await
                    && tx.send(Out::Json(reply)).is_err()
                {
                    break;
                }
            }
            In::Sound(b) if b.first() == Some(&1) => {
                hub.media_in_on(sid, Some(generation), &b[1..])
            }
            In::Close => break,
            _ => {}
        }
    }
    hub.detach(sid, generation, bye);
    if let Some(h) = &handset {
        hub.handset_seen(h);
    }
    let dropped = frames.dropped.load(std::sync::atomic::Ordering::Relaxed);
    if dropped > 0 {
        idn_open_protocol::log::log(&format!(
            "voice: {name}: {dropped} sound frames dropped so far (a slow or lost link)"
        ));
    }
    drop(tx);
    writer.abort();
}

/// The target a command names: `channel`, `user`, `all` or `call`.
fn target(v: &Value) -> Option<Target> {
    if let Some(c) = v["channel"].as_u64() {
        return Some(Target::Channel(c as u32));
    }
    if let Some(u) = v["user"].as_str() {
        return Some(Target::User(u.to_owned()));
    }
    if v["all"].as_bool() == Some(true) {
        return Some(Target::All);
    }
    v["call"].as_u64().map(Target::Call)
}

pub(crate) fn err(what: &str, e: impl ToString) -> Value {
    json!({"t": "error", "what": what, "error": e.to_string()})
}

async fn command(voice: &Voice, sid: u32, v: &Value, peer: IpAddr) -> Option<Value> {
    let hub = &voice.hub;
    let t = v["t"].as_str().unwrap_or_default();
    match t {
        "ping" => Some(json!({"t": "pong"})),
        "join" => {
            let c = v["channel"].as_u64()? as u32;
            let password = v["password"].as_str().map(str::to_owned);
            // A password: counted like a login (five wrong a minute per address, §V.3),
            // checked off the runtime, a few at once (E5-1).
            if password.is_some() {
                if !voice.logins.attempt(peer) {
                    return Some(err("join", "too many tries; wait a minute"));
                }
                let h = hub.clone();
                let r = crate::http::checks()
                    .run(move || h.join(sid, c, password.as_deref()))
                    .await
                    .unwrap_or_else(|| Err("join failed".into()));
                if r.is_ok() {
                    voice.logins.succeeded(peer);
                }
                return r.err().map(|e| err("join", e));
            }
            hub.join(sid, c, None).err().map(|e| err("join", e))
        }
        "leave" => {
            hub.leave(sid, v["channel"].as_u64()? as u32);
            None
        }
        "talk" => {
            if v["on"].as_bool() == Some(true) {
                let to = target(v)?;
                hub.talk_start(sid, to)
                    .err()
                    .filter(|e| e != "busy")
                    .map(|e| err("talk", e))
            } else {
                hub.talk_stop(sid);
                None
            }
        }
        "call" => hub
            .call(sid, v["user"].as_str()?)
            .err()
            .map(|e| err("call", e)),
        "answer" => hub
            .answer(
                sid,
                v["call"].as_u64()?,
                v["accept"].as_bool().unwrap_or(true),
            )
            .err()
            .map(|e| err("answer", e)),
        "hangup" => {
            hub.hangup(sid, v["call"].as_u64()?);
            None
        }
        "message" => {
            let to = target(v)?;
            hub.message(sid, &to, v["text"].as_str().unwrap_or_default())
                .err()
                .map(|e| err("message", e))
        }
        "history" => {
            let to = target(v)?;
            Some(match hub.history(sid, &to) {
                Ok(m) => {
                    json!({"t": "history", "to": {"channel": v["channel"], "user": v["user"]}, "messages": m})
                }
                Err(e) => err("history", e),
            })
        }
        "missed" => {
            let since = v["since_ms"].as_i64().unwrap_or(0);
            Some(json!({"t": "missed", "transmissions": hub.missed(sid, since)}))
        }
        "replay" => hub
            .replay(sid, v["id"].as_u64()?)
            .err()
            .map(|e| err("replay", e)),
        "settings" => {
            let s: UserSettings = serde_json::from_value(v["settings"].clone()).ok()?;
            Some(match hub.set_user_settings(sid, s.clone()) {
                Ok(()) => json!({"t": "settings", "settings": s}),
                Err(e) => err("settings", e),
            })
        }
        "users" => {
            // Signed-in users only: a guest got every site account's name (bug round E,
            // E5-4).
            if !hub.who(sid).is_some_and(|w| w.user) {
                return Some(err("users", "sign in to see the site's people"));
            }
            let mut names: Vec<String> = hub.host.users().into_iter().map(|u| u.name).collect();
            names.sort();
            Some(json!({"t": "users", "all": names}))
        }
        "rtc_offer" => {
            let Some(rtc) = voice.rtc.clone() else {
                return Some(err("rtc", "WebRTC is off on this server"));
            };
            let sdp = v["sdp"].as_str()?.to_owned();
            let r = tokio::task::spawn_blocking(move || rtc.offer(sid, sdp, peer))
                .await
                .unwrap_or_else(|e| Err(e.to_string()));
            Some(match r {
                Ok(sdp) => json!({"t": "rtc_answer", "sdp": sdp}),
                Err(e) => err("rtc", e),
            })
        }
        "rtc_off" => {
            hub.set_rtc_media(sid, false);
            None
        }
        // A handset's SOS button (§V.13): to every dispatcher until acknowledged.
        "sos" => {
            let position = v.get("position").filter(|p| !p.is_null()).cloned();
            Some(
                match hub.sos(sid, position, v["text"].as_str().unwrap_or_default()) {
                    Ok(id) => json!({"t": "sos_sent", "id": id}),
                    Err(e) => err("sos", e),
                },
            )
        }
        "sos_ack" => {
            let id = v["id"].as_u64()?;
            let by = hub.who(sid)?.name;
            hub.sos_ack(id, &by, Some(sid))
                .err()
                .map(|e| err("sos_ack", e))
        }
        _ => None,
    }
}
