//! A fake X32 for tests: a UDP responder on 127.0.0.1 answering the documented messages
//! (see `proto` for the source) with a small default desk, and pushing changes to its
//! `/xremote` subscribers while their subscription lasts.

use super::proto;
use super::*;
use std::collections::HashMap;

/// A fake X32 on 127.0.0.1.
pub(crate) struct Fake {
    pub(crate) addr: SocketAddr,
    pub(crate) inner: Arc<FakeInner>,
    thread: Option<std::thread::JoinHandle<()>>,
}

pub(crate) struct FakeInner {
    socket: UdpSocket,
    model: String,
    pub(crate) values: Mutex<HashMap<String, Arg>>,
    /// Everything received (while alive).
    log: Mutex<Vec<Message>>,
    /// `/xremote` subscribers and until when.
    subs: Mutex<Vec<(SocketAddr, Instant)>>,
    /// How long `/xremote` lasts (the real one: 10 s).
    sub_life: Duration,
    alive: AtomicBool,
    stop: AtomicBool,
}

impl FakeInner {
    fn send(&self, to: SocketAddr, m: &Message) {
        let _ = self.socket.send_to(&encode(m), to);
    }

    fn handle(&self, m: Message, from: SocketAddr) {
        self.log.lock_ok().push(m.clone());
        match m.addr.as_str() {
            "/info" => self.send(
                from,
                &Message::new(
                    "/info",
                    ["V2.05", "osc-server", &self.model, "4.06"]
                        .into_iter()
                        .map(|s| Arg::Str(s.into()))
                        .collect(),
                ),
            ),
            "/xinfo" => self.send(
                from,
                &Message::new(
                    "/xinfo",
                    ["127.0.0.1", "X32-02-4A-53", &self.model, "4.06"]
                        .into_iter()
                        .map(|s| Arg::Str(s.into()))
                        .collect(),
                ),
            ),
            "/xremote" => {
                let mut s = self.subs.lock_ok();
                s.retain(|(a, _)| *a != from);
                s.push((from, Instant::now() + self.sub_life));
            }
            addr if m.args.is_empty() => {
                let v = self.values.lock_ok().get(addr).cloned();
                if let Some(v) = v {
                    self.send(from, &Message::new(addr, vec![v]));
                }
            }
            addr => {
                // A set: kept, and echoed to the other subscribers (not the sender).
                self.values.lock_ok().insert(addr.into(), m.args[0].clone());
                self.push_except(&m, Some(from));
            }
        }
    }

    fn push_except(&self, m: &Message, except: Option<SocketAddr>) {
        let now = Instant::now();
        let subs: Vec<SocketAddr> = self
            .subs
            .lock_ok()
            .iter()
            .filter(|(a, until)| *until > now && Some(*a) != except)
            .map(|(a, _)| *a)
            .collect();
        for to in subs {
            self.send(to, m);
        }
    }
}

impl Fake {
    pub(crate) fn start(model: &str, sub_life: Duration) -> Fake {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(5)))
            .unwrap();
        let addr = socket.local_addr().unwrap();
        let mut values = HashMap::new();
        let mut put = |a: String, v: Arg| {
            values.insert(a, v);
        };
        // Card: AN1-8, A1-8, B1-8, UOUT1-8.
        for (b, v) in proto::CARD_BLOCKS.iter().zip([0, 4, 10, 26]) {
            put((*b).into(), Arg::Int(v));
        }
        // User outs 1…8: Local 5, AES50-A 8, Output 2, off, Card In 2, P16 3, off, off.
        for (n, v) in [5, 40, 170, 0, 130, 187, 0, 0].into_iter().enumerate() {
            put(proto::user_out(n as u8 + 1), Arg::Int(v));
        }
        // Output 2: channel 5's direct out (P16 3's source is left unanswered).
        put(proto::output_src(2), Arg::Int(30));
        // Channel k takes headamp k-1, but channel 3 takes none (-1).
        for ch in 1..=32u8 {
            let v = if ch == 3 { -1 } else { i32::from(ch) - 1 };
            put(proto::channel_headamp(ch), Arg::Int(v));
            put(proto::channel_invert(ch), Arg::Int(i32::from(ch == 2)));
            put(proto::channel_source(ch), Arg::Int(i32::from(ch)));
        }
        // Every headamp at +6 dB (0.25), phantom off.
        for ha in 0..128u8 {
            put(proto::headamp_gain(ha), Arg::Float(0.25));
            put(proto::headamp_phantom(ha), Arg::Int(0));
        }
        put("/-stat/aes50/A".into(), Arg::Str("A".into()));
        put("/-stat/aes50/B".into(), Arg::Str("".into()));
        let inner = Arc::new(FakeInner {
            socket,
            model: model.into(),
            values: Mutex::new(values),
            log: Mutex::new(Vec::new()),
            subs: Mutex::new(Vec::new()),
            sub_life,
            alive: AtomicBool::new(true),
            stop: AtomicBool::new(false),
        });
        let i = inner.clone();
        let thread = std::thread::spawn(move || {
            let mut buf = vec![0u8; 4096];
            while !i.stop.load(Ordering::Relaxed) {
                let Ok((n, from)) = i.socket.recv_from(&mut buf) else {
                    continue;
                };
                if !i.alive.load(Ordering::Relaxed) {
                    continue;
                }
                for m in decode(&buf[..n]).unwrap() {
                    i.handle(m, from);
                }
            }
        });
        Fake {
            addr,
            inner,
            thread: Some(thread),
        }
    }

    /// A change made "on the desk": kept and sent to the subscribers.
    pub(crate) fn push(&self, addr: &str, v: Arg) {
        self.inner.values.lock_ok().insert(addr.into(), v.clone());
        self.inner.push_except(&Message::new(addr, vec![v]), None);
    }

    pub(crate) fn value(&self, addr: &str) -> Option<Arg> {
        self.inner.values.lock_ok().get(addr).cloned()
    }

    pub(crate) fn received(&self, addr: &str) -> Vec<Message> {
        self.inner
            .log
            .lock_ok()
            .iter()
            .filter(|m| m.addr == addr)
            .cloned()
            .collect()
    }

    pub(crate) fn log(&self) -> Vec<Message> {
        self.inner.log.lock_ok().clone()
    }

    pub(crate) fn set_alive(&self, on: bool) {
        self.inner.alive.store(on, Ordering::Relaxed);
        if !on {
            self.inner.subs.lock_ok().clear();
        }
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
