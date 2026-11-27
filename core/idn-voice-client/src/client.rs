//! The Rust API: [`Client::connect`], the commands, the microphone in and the speaker out.

use crate::LockOk as _;
use crate::config::{Config, ResumeState};
use crate::event::{Event, Transport};
use crate::framing::mic_message;
use crate::mic::to_f32;
use crate::outbox::Item;
use crate::session::{self, Shared};
use serde_json::{Value, json};
use std::io;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

/// Who a talk or a message is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A channel, by id.
    Channel(u32),
    /// A person, by key (`driver:…`, `handset:…`, or a site user's name): instant talk, a
    /// direct message.
    User(String),
    /// Every channel this session may talk in (dispatch).
    All,
    /// A call, by id.
    Call(u64),
}

impl From<u32> for Target {
    fn from(c: u32) -> Target {
        Target::Channel(c)
    }
}

impl From<&str> for Target {
    fn from(u: &str) -> Target {
        Target::User(u.to_owned())
    }
}

impl Target {
    fn put(&self, v: &mut Value) {
        match self {
            Target::Channel(c) => v["channel"] = json!(c),
            Target::User(u) => v["user"] = json!(u),
            Target::All => v["all"] = json!(true),
            Target::Call(c) => v["call"] = json!(c),
        }
    }
}

/// How the client is doing.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Stats {
    /// The connection's transport (none while connecting again).
    pub transport: Option<Transport>,
    /// The app holds a talk (the microphone is sent).
    pub talking: bool,
    /// Talkers' streams being played, and the sound frames received.
    pub streams: usize,
    pub packets: u64,
    /// The fullest stream's buffer and its target, ms.
    pub buffered_ms: u32,
    pub target_ms: u32,
    /// Microphone packets held (the link down, or sent and not yet confirmed), and
    /// dropped past `hold_mic_s` (or with the talk refused or taken).
    pub mic_held: usize,
    pub mic_dropped: u64,
    /// Frames lost (concealed), stalls (the buffer grew), frames late, again, dropped.
    pub lost: u64,
    pub stalls: u64,
    pub late: u64,
    pub duplicates: u64,
    pub dropped: u64,
}

/// A Voice client: one session with the server, kept across dropped links.
///
/// Every method takes `&self` and may be called from any thread. [`Client::pull_speaker_f32`]
/// (or `_i16`) is for the app's audio output thread: it decodes and mixes there, holding
/// a lock only briefly, never waiting for the network. Events come on the library's own
/// threads, one at a time.
pub struct Client {
    sh: Arc<Shared>,
    handle: tokio::runtime::Handle,
    rt: Mutex<Option<tokio::runtime::Runtime>>,
    done: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl Client {
    /// Starts the session in the background and returns at once: [`Event::Connected`]
    /// and [`Event::Hello`] follow, or [`Event::Reconnecting`] while it can't connect.
    /// `events` runs on the library's threads; it must not block for long (hand the event
    /// to the app's own thread). An error only for a configuration that can't work.
    pub fn connect(
        mut config: Config,
        events: impl Fn(Event) + Send + Sync + 'static,
    ) -> io::Result<Client> {
        config
            .check()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        // Two workers (the session, its connection's reader and writer, QUIC's driver),
        // two more at most for name lookups: a handset's small processor.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(2)
            .thread_name("idn-voice-client")
            .enable_all()
            .build()?;
        let (close_tx, close_rx) = watch::channel(false);
        let sh = Arc::new(Shared::new(config, Box::new(events), close_tx));
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let s2 = sh.clone();
        rt.spawn(async move {
            session::run(s2, close_rx).await;
            let _ = done_tx.send(());
        });
        Ok(Client {
            sh,
            handle: rt.handle().clone(),
            rt: Mutex::new(Some(rt)),
            done: Mutex::new(Some(done_rx)),
        })
    }

    /// A command, in order with the talk; kept until confirmed and sent again after a
    /// drop (a command sent in the second before a silent drop may arrive twice).
    fn send(&self, v: Value) {
        self.sh.outbox.lock_ok().push(Item::Cmd(v.to_string()));
        self.sh.out_ready.notify_one();
    }

    /// Joins a channel (`password` for a password channel).
    pub fn join(&self, channel: u32, password: Option<&str>) {
        let mut v = json!({"t": "join", "channel": channel});
        if let Some(p) = password {
            v["password"] = json!(p);
        }
        self.send(v);
    }

    pub fn leave(&self, channel: u32) {
        self.send(json!({"t": "leave", "channel": channel}));
    }

    /// Starts talking to `to` (a channel id, or a [`Target`]): the microphone is sent
    /// from now on, until [`Client::talk_stop`] or the server ends the talk
    /// ([`Event::Talk`] off). Through a dead zone the talk goes on: its packets are held
    /// (`hold_mic_s`) and sent after the reconnect, the talk started again first.
    pub fn talk_start(&self, to: impl Into<Target>) {
        let mut v = json!({"t": "talk", "on": true});
        to.into().put(&mut v);
        self.talk(v);
    }

    /// A talk command, in order with the microphone's packets (the outbox), numbered.
    /// (Under the microphone's lock: no packet of the talk before goes after it.)
    fn talk(&self, v: Value) {
        let mut mic = self.sh.mic.lock_ok();
        mic.clear();
        let json = v.to_string();
        let mut ob = self.sh.outbox.lock_ok();
        let id = ob.new_talk(&json);
        self.sh.state.lock_ok().talk = Some((id, v));
        ob.push(Item::Talk { id, json, on: true });
        drop(ob);
        drop(mic);
        self.sh.out_ready.notify_one();
    }

    /// Stops talking: the last part of a frame is sent, then the talk ends (after what
    /// is held, if the link is down). (The server's talk has no channel: it ends
    /// whichever is on.)
    pub fn talk_stop(&self) {
        let mut mic = self.sh.mic.lock_ok();
        let held = self.sh.state.lock_ok().talk.take().map(|(id, _)| id);
        let outbox = &self.sh.outbox;
        match held {
            Some(talk) => mic.finish(|p| {
                outbox.lock_ok().push(Item::Sound {
                    talk,
                    bytes: mic_message(p),
                })
            }),
            None => mic.clear(),
        }
        let json = json!({"t": "talk", "on": false}).to_string();
        outbox.lock_ok().push(Item::Talk {
            id: held.unwrap_or(0),
            json,
            on: false,
        });
        drop(mic);
        self.sh.out_ready.notify_one();
    }

    /// A text message to a channel or a person (1–2,000 characters).
    pub fn message(&self, to: impl Into<Target>, text: &str) {
        let mut v = json!({"t": "message", "text": text});
        to.into().put(&mut v);
        self.send(v);
    }

    /// The SOS button: `position` (latitude, longitude in degrees) and `text` if known.
    pub fn sos(&self, position: Option<(f64, f64)>, text: Option<&str>) {
        let mut v = json!({"t": "sos"});
        if let Some((lat, lon)) = position {
            v["position"] = json!({"lat": lat, "lon": lon});
        }
        if let Some(t) = text {
            v["text"] = json!(t);
        }
        self.send(v);
    }

    /// Calls a person (by key).
    pub fn call(&self, user: &str) {
        self.send(json!({"t": "call", "user": user}));
    }

    /// Answers a call (`accept` false: declines it).
    pub fn answer(&self, call: u64, accept: bool) {
        self.send(json!({"t": "answer", "call": call, "accept": accept}));
    }

    pub fn hangup(&self, call: u64) {
        self.send(json!({"t": "hangup", "call": call}));
    }

    /// Any command as JSON (VOICE-CLIENT.md §4.3): an object with `t`. A `talk` goes as
    /// [`Client::talk_start`]/[`Client::talk_stop`] do (the microphone follows it); a
    /// `talk` on needs its target (`channel`, `user`, `all` or `call`). `bye` is refused:
    /// use [`Client::close`]. A `ping` is ignored: the library pings (its pongs confirm
    /// what was sent). Commands are kept until confirmed and sent again after a drop.
    pub fn send_json(&self, raw: &str) -> io::Result<()> {
        let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidInput, m.to_owned());
        let v: Value = serde_json::from_str(raw).map_err(|e| bad(&format!("not JSON: {e}")))?;
        let Some(t) = v["t"].as_str() else {
            return Err(bad("a JSON object with \"t\""));
        };
        if raw.len() > crate::MAX_FRAME - 1 {
            return Err(bad("more than 64 KiB"));
        }
        if t == "bye" {
            return Err(bad("sign off with close, not bye"));
        }
        let targeted = ["channel", "user", "all", "call"]
            .iter()
            .any(|k| !v[*k].is_null());
        match (t, v["on"].as_bool()) {
            ("talk", Some(true)) if !targeted => {
                return Err(bad("a talk needs channel, user, all or call"));
            }
            ("talk", Some(true)) => self.talk(v),
            ("talk", _) => self.talk_stop(),
            ("ping", _) => {}
            _ => self.send(v),
        }
        Ok(())
    }

    /// The microphone: `pcm` mono at `rate` (48000; or 8000, 12000, 16000, 24000), any
    /// length. Coded and sent in 20 ms frames while talking; dropped otherwise. While the
    /// link is down the packets are held (`hold_mic_s`, the oldest dropped past it,
    /// counted in [`Stats`]) and sent when it is back. Call it from the app's recording
    /// thread as samples come.
    pub fn push_mic_f32(&self, pcm: &[f32], rate: u32) -> io::Result<()> {
        if !crate::MIC_RATES.contains(&rate) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("microphone at {rate} Hz: give 48000 (or 8000, 12000, 16000, 24000)"),
            ));
        }
        // (The talk read under the microphone's lock: a talk_stop can't come between.)
        let mut mic = self.sh.mic.lock_ok();
        let talk = self.sh.state.lock_ok().talk.as_ref().map(|(id, _)| *id);
        let Some(talk) = talk else {
            mic.clear();
            return Ok(());
        };
        // Holding off (`hold_mic_s` 0) and the link down: counted, not kept.
        let hold = self.sh.cfg.hold_mic_s > 0 || self.sh.connected.load(Ordering::SeqCst);
        let outbox = &self.sh.outbox;
        let r = mic.push(pcm, rate, |p| {
            let mut ob = outbox.lock_ok();
            if hold {
                ob.push(Item::Sound {
                    talk,
                    bytes: mic_message(p),
                });
            } else {
                ob.drop_one();
            }
        });
        drop(mic);
        self.sh.out_ready.notify_one();
        r
    }

    /// As [`Client::push_mic_f32`], 16-bit samples.
    pub fn push_mic_i16(&self, pcm: &[i16], rate: u32) -> io::Result<()> {
        let mut f = Vec::with_capacity(pcm.len());
        to_f32(pcm, &mut f);
        self.push_mic_f32(&f, rate)
    }

    /// The speaker: fills `out` with the next samples, 48 kHz mono (−1…+1), every
    /// talker heard mixed. Silence when nobody talks. For the audio output callback.
    pub fn pull_speaker_f32(&self, out: &mut [f32]) {
        self.sh.speaker.lock_ok().pull(out);
    }

    /// As [`Client::pull_speaker_f32`], 16-bit samples.
    pub fn pull_speaker_i16(&self, out: &mut [i16]) {
        let mut f = [0.0f32; 480];
        for chunk in out.chunks_mut(f.len()) {
            let f = &mut f[..chunk.len()];
            self.pull_speaker_f32(f);
            for (o, v) in chunk.iter_mut().zip(f.iter()) {
                *o = (v * 32_767.0).round().clamp(-32_768.0, 32_767.0) as i16;
            }
        }
    }

    /// A channel's volume (1 as it comes, 0 silent, up to 4).
    pub fn set_channel_volume(&self, channel: u32, gain: f32) {
        self.sh.speaker.lock_ok().set_volume(channel, gain);
    }

    /// The device's network changed (Android's `ConnectivityManager` callback): QUIC
    /// moves to a fresh socket and goes on (connection migration); a WebSocket on the old
    /// network is given up and the session resumed on the new one at once.
    pub fn network_changed(&self) {
        // (A socket is made inside the runtime: tokio's needs its reactor.)
        {
            let _in = self.handle.enter();
            self.sh.endpoints.lock_ok().rebind();
        }
        self.sh.net_gen.fetch_add(1, Ordering::SeqCst);
        self.sh.net.notify_one();
        self.sh.net_drive.notify_one();
    }

    /// The transport now (none while connecting).
    pub fn transport(&self) -> Option<Transport> {
        self.sh.state.lock_ok().transport
    }

    /// What to keep to come back to this session after the app restarts (within the
    /// site's `resume_s`): give it as `resume` and `have` in the next configuration.
    pub fn resume_state(&self) -> Option<ResumeState> {
        self.sh.state.lock_ok().resume()
    }

    pub fn stats(&self) -> Stats {
        let (transport, talking) = {
            let st = self.sh.state.lock_ok();
            (st.transport, st.talk.is_some())
        };
        let s = self.sh.speaker.lock_ok().stats();
        let (mic_held, mic_dropped) = {
            let ob = self.sh.outbox.lock_ok();
            (ob.held(), ob.dropped)
        };
        Stats {
            transport,
            talking,
            mic_held,
            mic_dropped,
            streams: s.streams,
            packets: s.packets,
            buffered_ms: s.buffered_ms,
            target_ms: s.target_ms,
            lost: s.lost,
            stalls: s.stalls,
            late: s.late,
            duplicates: s.duplicates,
            dropped: s.dropped,
        }
    }

    /// Signs off (`bye`: the session ends now, not waiting for a resume) and stops the
    /// library's threads. No event comes after it returns. It waits up to a second or
    /// two for the `bye` to go; from inside the event callback it doesn't wait.
    pub fn close(&self) {
        if self.sh.close.send_replace(true) {
            return;
        }
        let inside = session::in_callback(&self.sh);
        if !inside && let Some(done) = self.done.lock_ok().take() {
            let _ = done.recv_timeout(Duration::from_secs(2));
        }
        // From here no event starts; one under way (on another thread) is waited for.
        self.sh.closed.store(true, Ordering::SeqCst);
        if !inside {
            drop(self.sh.cb.lock_ok());
        }
        // The runtime stops on a thread of its own: this may be one of its threads (the
        // callback), or a thread inside another runtime, where it can't be dropped.
        if let Some(rt) = self.rt.lock_ok().take() {
            let _ = std::thread::Builder::new()
                .name("idn-voice-client-stop".into())
                .spawn(move || rt.shutdown_timeout(Duration::from_secs(1)));
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.close();
    }
}
