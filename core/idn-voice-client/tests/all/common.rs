//! The tests' pieces: the real Voice server in-process (plain HTTP on loopback, plain
//! handsets allowed, as idn-voice's own tests), handsets provisioned on it, a TCP relay
//! that can cut every connection (a dropped link), events gathered, an "audio thread"
//! pulling the speaker in real time, a microphone pushing a tone.

use idn_common::sync::LockOk as _;
use idn_common::testing::TempDir;
use idn_voice::{Host, User, Voice, VoiceConfig};
use idn_voice_client::{Client, Event};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct TestHost;

fn users() -> Vec<User> {
    vec![User {
        name: "ann".into(),
        roles: vec!["admin".into()],
        admin: true,
    }]
}

impl Host for TestHost {
    fn session_user(&self, _: &axum::http::HeaderMap) -> Option<User> {
        None
    }
    fn login(&self, name: &str, password: &str, _: std::net::IpAddr) -> Option<User> {
        users()
            .into_iter()
            .find(|u| u.name == name && password == format!("pw-{name}"))
    }
    fn users(&self) -> Vec<User> {
        users()
    }
    fn audit(&self, _: &str, _: &str, _: Value) {}
}

/// A Voice server on its own runtime.
pub struct Server {
    pub addr: SocketAddr,
    /// (Read by the QUIC server tests, behind their feature.)
    #[allow(dead_code)]
    pub voice: Voice,
    pub rt: tokio::runtime::Runtime,
    /// An administrator's cookie.
    pub admin: String,
    _dir: TempDir,
}

impl Server {
    pub fn start() -> Server {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let dir = TempDir::new("voice-client");
        let mut cfg = VoiceConfig::new(dir.path().to_path_buf());
        cfg.udp = None;
        cfg.cert_sha256 = None;
        cfg.urls = vec![];
        let (addr, voice) = rt.block_on(async {
            let v = Voice::start(cfg, Arc::new(TestHost)).unwrap();
            // (Plain HTTP on loopback: handsets allowed without TLS.)
            let mut s = v.hub.settings();
            s.plain_handsets = true;
            v.hub.put_settings(s).unwrap();
            let app = axum::Router::new().nest("/voice", v.router());
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = l.local_addr().unwrap();
            tokio::spawn(async move {
                let _ =
                    axum::serve(l, app.into_make_service_with_connect_info::<SocketAddr>()).await;
            });
            (addr, v)
        });
        let mut s = Server {
            addr,
            voice,
            rt,
            admin: String::new(),
            _dir: dir,
        };
        let (st, cookie, _) = s.http(
            "POST",
            "/voice/api/login",
            &json!({"user": "ann", "password": "pw-ann"}),
        );
        assert_eq!(st, 200);
        s.admin = cookie;
        s
    }

    /// A plain HTTP/1.1 request as the administrator: (status, Set-Cookie, body).
    pub fn http(&self, method: &str, path: &str, body: &Value) -> (u16, String, Value) {
        let (addr, cookie, b) = (self.addr, self.admin.clone(), body.to_string());
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nX-IDN-Voice: 1\r\nCookie: {cookie}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}",
            b.len()
        );
        let text = self.rt.block_on(async move {
            let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
            s.write_all(req.as_bytes()).await.unwrap();
            let mut out = Vec::new();
            s.read_to_end(&mut out).await.unwrap();
            String::from_utf8_lossy(&out).into_owned()
        });
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let status = head
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let cookie = head
            .lines()
            .find_map(|l| {
                l.strip_prefix("set-cookie: ")
                    .or_else(|| l.strip_prefix("Set-Cookie: "))
            })
            .map(|c| c.split(';').next().unwrap_or("").to_owned())
            .unwrap_or_default();
        (
            status,
            cookie,
            serde_json::from_str(body.trim()).unwrap_or(Value::Null),
        )
    }

    /// An open Walkie-Talkie channel: its id.
    pub fn channel(&self, name: &str) -> u32 {
        let (st, _, v) = self.http(
            "POST",
            "/voice/api/channels",
            &json!({"name": name, "kind": "walkie", "access": "open"}),
        );
        assert_eq!(st, 200, "{v}");
        v["id"].as_u64().unwrap() as u32
    }

    /// A handset given its credential at once (a fleet app's provisioning): the answer.
    pub fn provision(&self, name: &str) -> Value {
        let (st, _, v) = self.http(
            "POST",
            "/voice/api/handsets",
            &json!({"name": name, "issue": "credential"}),
        );
        assert_eq!(st, 200, "{v}");
        v
    }

    /// The WebSocket's base address.
    pub fn url(&self) -> String {
        format!("http://{}/voice", self.addr)
    }
}

/// A TCP relay to `to` that can cut its connections and refuse new ones (a link down),
/// or go silent on them, open but forwarding nothing (a cellular link dead without a
/// word).
/// A relayed connection's task and its "forward nothing" flag.
type RelayConn = (tokio::task::JoinHandle<()>, Arc<AtomicBool>);

pub struct Relay {
    pub addr: SocketAddr,
    up: Arc<AtomicBool>,
    conns: Arc<Mutex<Vec<RelayConn>>>,
}

/// One way of a relayed connection: what comes is passed on, or dropped once silent.
async fn pipe(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    silent: Arc<AtomicBool>,
) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = match from.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        if silent.load(Ordering::SeqCst) {
            continue;
        }
        if to.write_all(&buf[..n]).await.is_err() {
            return;
        }
    }
}

impl Relay {
    pub fn start(rt: &tokio::runtime::Runtime, to: SocketAddr) -> Relay {
        let l = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let addr = l.local_addr().unwrap();
        let up = Arc::new(AtomicBool::new(true));
        let conns: Arc<Mutex<Vec<RelayConn>>> = Arc::default();
        let (up2, conns2) = (up.clone(), conns.clone());
        rt.spawn(async move {
            while let Ok((c, _)) = l.accept().await {
                if !up2.load(Ordering::SeqCst) {
                    continue;
                }
                let silent = Arc::new(AtomicBool::new(false));
                let s2 = silent.clone();
                let h = tokio::spawn(async move {
                    if let Ok(s) = tokio::net::TcpStream::connect(to).await {
                        let (cr, cw) = c.into_split();
                        let (sr, sw) = s.into_split();
                        tokio::join!(pipe(cr, sw, s2.clone()), pipe(sr, cw, s2));
                    }
                });
                conns2.lock_ok().push((h, silent));
            }
        });
        Relay { addr, up, conns }
    }

    /// Every connection open but silent from now on: nothing passes either way.
    pub fn silence(&self) {
        for (_, s) in self.conns.lock_ok().iter() {
            s.store(true, Ordering::SeqCst);
        }
    }

    /// Every connection cut, new ones refused.
    pub fn cut(&self) {
        self.up.store(false, Ordering::SeqCst);
        for (h, _) in self.conns.lock_ok().drain(..) {
            h.abort();
        }
    }

    pub fn restore(&self) {
        self.up.store(true, Ordering::SeqCst);
    }

    pub fn url(&self) -> String {
        format!("http://{}/voice", self.addr)
    }
}

/// Events a client gave, in order.
#[derive(Clone, Default)]
pub struct Events(Arc<Mutex<Vec<Event>>>);

impl Events {
    /// The callback that gathers them.
    pub fn sink(&self) -> impl Fn(Event) + Send + Sync + 'static {
        let all = self.0.clone();
        move |e| all.lock_ok().push(e)
    }

    pub fn all(&self) -> Vec<Event> {
        self.0.lock_ok().clone()
    }

    /// The first (from `from` on) for which `f` holds, within `within`: its index and it.
    pub fn wait(
        &self,
        what: &str,
        from: usize,
        within: Duration,
        f: impl Fn(&Event) -> bool,
    ) -> (usize, Event) {
        let until = Instant::now() + within;
        loop {
            let all = self.all();
            if let Some((i, e)) = all.iter().enumerate().skip(from).find(|(_, e)| f(e)) {
                return (i, e.clone());
            }
            assert!(
                Instant::now() < until,
                "no {what} within {within:?}: {all:#?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The `hello` (from `from` on): its index and the message.
    pub fn hello(&self, from: usize) -> (usize, bool, Value) {
        match self.wait("hello", from, Duration::from_secs(10), |e| {
            matches!(e, Event::Hello { .. })
        }) {
            (i, Event::Hello { resumed, hello }) => (i, resumed, hello),
            _ => unreachable!(),
        }
    }
}

/// A client from a provisioning answer and these addresses, with its events.
pub fn client(answer: &Value, extra: Value) -> (Arc<Client>, Events) {
    let mut cfg = json!({
        "credential": answer["credential"],
        "app": "voice-client tests", "version": "1",
    });
    for (k, v) in extra.as_object().unwrap() {
        cfg[k] = v.clone();
    }
    let ev = Events::default();
    let c = Client::connect(
        idn_voice_client::Config::from_json(&cfg.to_string()).unwrap(),
        ev.sink(),
    )
    .unwrap();
    (Arc::new(c), ev)
}

/// The app's audio output: the speaker pulled every 20 ms in real time, kept.
pub struct Ear {
    pub samples: Arc<Mutex<Vec<i16>>>,
    pub started: Instant,
    stop: Arc<AtomicBool>,
    t: Option<std::thread::JoinHandle<()>>,
}

impl Ear {
    pub fn start(c: Arc<Client>) -> Ear {
        let samples: Arc<Mutex<Vec<i16>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (s2, stop2) = (samples.clone(), stop.clone());
        let started = Instant::now();
        let t = std::thread::spawn(move || {
            let mut block = [0i16; 960];
            let mut n = 0u32;
            while !stop2.load(Ordering::SeqCst) {
                c.pull_speaker_i16(&mut block);
                s2.lock_ok().extend_from_slice(&block);
                n += 1;
                let next = started + Duration::from_millis(20) * n;
                std::thread::sleep(next.saturating_duration_since(Instant::now()));
            }
        });
        Ear {
            samples,
            started,
            stop,
            t: Some(t),
        }
    }

    pub fn samples(&self) -> Vec<i16> {
        self.samples.lock_ok().clone()
    }
}

impl Drop for Ear {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.t.take() {
            let _ = t.join();
        }
    }
}

/// `ms` of a 440 Hz tone at 0.3 of full scale pushed as a microphone would, 20 ms at a
/// time in real time: when it started.
pub fn speak(c: &Client, ms: u32) -> Instant {
    let start = Instant::now();
    let frames = ms / 20;
    for k in 0..frames {
        let pcm: Vec<i16> = (0..960)
            .map(|i| {
                let t = (k * 960 + i) as f32 / 48_000.0;
                (0.3 * 32_767.0 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()) as i16
            })
            .collect();
        c.push_mic_i16(&pcm, 48_000).unwrap();
        let next = start + Duration::from_millis(20) * (k + 1);
        std::thread::sleep(next.saturating_duration_since(Instant::now()));
    }
    start
}

/// Each 20 ms block's RMS, 0 to 1.
pub fn block_rms(s: &[i16]) -> Vec<f64> {
    s.chunks(960)
        .map(|b| {
            (b.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / b.len() as f64).sqrt()
                / 32_768.0
        })
        .collect()
}

/// Sign changes a second over `s`, at 48 kHz (a 440 Hz tone: 880).
pub fn crossings_per_s(s: &[i16]) -> f64 {
    let n = s.windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count();
    n as f64 * 48_000.0 / s.len() as f64
}
