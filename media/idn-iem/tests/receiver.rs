//! The phone mixing library against a real desk in the test (idn-mixer's Console, its IEM
//! flow, mixes, push channel and tokens; idn-iem.md §I.8): what a musician's phone does,
//! over loopback.

use idn_common::sync::LockOk as _;
use idn_common::testing::{TempDir, wait_for, wait_until};
use idn_iem::receiver::{Config, Error, Event, Receiver, Transport, Trust};
use idn_mixer::{Console, ConsoleConfig};
use idn_open_protocol::trust::pki::KeyFile;
use idn_open_protocol::trust::token::{self, Claims, Scope};
use idn_open_protocol::web::{
    self, Handler, PasswordGate, Request, Response, SiteKeys, TokenCheck,
};
use serde_json::{Value, json};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The test tones: desk input (and channel) n plays `TONES[n - 1]` Hz at 0.25.
const TONES: [f32; 4] = [500.0, 1500.0, 2400.0, 3600.0];
const AMP: f32 = 0.25;

#[derive(Default)]
struct Opts {
    /// Site tokens only (none from this machine either), checked by a site key of the
    /// test's own.
    tokens: bool,
    /// The API over HTTPS with the desk's own certificate.
    tls: bool,
    /// Channels 3 and 4 a linked pair (else channel 4 takes the talkback input).
    pair: bool,
    /// The desk's session timeout.
    timeout: Option<Duration>,
    /// With `tls`: the desk's machine in a site of the test's own, its network port
    /// presenting the site's certificate (idn-extensions.md §E.10.11).
    site: bool,
}

/// A site in the test and the desk's machine in it.
struct InSite {
    _authority: idn_open_protocol::trust::Authority,
    _ds: idn_open_protocol::discovery::DiscoveryServer,
    _member: Arc<idn_open_protocol::trust::Enrollment>,
    /// The site CA certificate (DER).
    ca: Vec<u8>,
}

impl InSite {
    fn new(dir: &std::path::Path) -> InSite {
        use idn_open_protocol::trust::{
            Authority, AuthorityConfig, DeviceRole, Enrollment, EnrollmentConfig,
        };
        let authority = Authority::open(AuthorityConfig {
            state_dir: dir.join("authority"),
            passphrase: Some("choir".into()),
        })
        .unwrap();
        let ds = idn_open_protocol::discovery::DiscoveryServer::start(0, Some(authority.clone()))
            .unwrap();
        let member = Enrollment::start(EnrollmentConfig {
            discovery_server: format!("127.0.0.1:{}", ds.port()),
            state_dir: dir.join("machine"),
            name: "DESK (App)".into(),
            role: DeviceRole::App,
            passphrase: Some("choir".into()),
            admin_code: None,
        })
        .unwrap();
        wait_for(
            "the site's certificate for the desk's machine",
            Duration::from_secs(10),
            || member.lan_cert().is_some(),
        );
        InSite {
            ca: authority.ca_der(),
            _authority: authority,
            _ds: ds,
            _member: member,
        }
    }
}

/// A desk with channels 1–3 (or 1–4) marked IEM, tones on its inputs, its API served,
/// and a mix "drums": channel 1 at −6 dB hard left, 2 at 0 dB hard right, the pair (or 3)
/// at 0 dB centre.
struct Desk {
    c: Arc<Console>,
    port: u16,
    stop: Arc<AtomicBool>,
    feeding: Arc<AtomicBool>,
    /// Removed when the desk is (after its console).
    _dir: TempDir,
    key: KeyFile,
    site: String,
    cert: Option<(Vec<u8>, String)>,
    in_site: Option<InSite>,
}

impl Drop for Desk {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.feeding.store(false, Ordering::Relaxed);
    }
}

impl Desk {
    fn new(tag: &str, o: Opts) -> Desk {
        let dir = TempDir::new(&format!("iem-{tag}"));
        let key = KeyFile::generate().unwrap();
        let site = key.fingerprint()[..16].to_owned();
        let check = {
            let (public, s2) = (key.public, site.clone());
            TokenCheck::new("desk", move || {
                Some(SiteKeys {
                    ca_public: public,
                    site: s2.clone(),
                    machine: "m1".into(),
                    options: Default::default(),
                })
            })
        };
        let mut cfg = ConsoleConfig {
            name: format!("IEM {tag}"),
            channels: 4,
            inputs: 4,
            mdns: Some(false),
            latency_ms: 60,
            adaptive: false,
            ..Default::default()
        };
        cfg.iem.port = Some(0);
        if o.tokens {
            cfg.token_check = Some(check.clone());
            cfg.iem.trust_loopback = false;
        }
        if let Some(t) = o.timeout {
            cfg.iem.timeout = t;
        }
        let c = Arc::new(Console::start(cfg).unwrap());
        // The tones, in real time, 40 ms ahead.
        let feeding = Arc::new(AtomicBool::new(true));
        {
            let (c, on) = (c.clone(), feeding.clone());
            std::thread::spawn(move || {
                let start = Instant::now();
                let mut pushed = 0usize;
                while on.load(Ordering::Relaxed) {
                    let want = ((start.elapsed().as_secs_f64() + 0.04) * 48_000.0) as usize;
                    while pushed < want {
                        let block: Vec<Vec<f32>> = TONES
                            .iter()
                            .map(|f| {
                                (pushed..pushed + 96)
                                    .map(|i| {
                                        AMP * (2.0 * std::f32::consts::PI * f * i as f32 / 48_000.0)
                                            .sin()
                                    })
                                    .collect()
                            })
                            .collect();
                        let _ = c.feed("tones", 1, 48_000, &block);
                        pushed += 96;
                    }
                    std::thread::sleep(Duration::from_millis(3));
                }
            });
        }
        c.channel(1, &json!({"name": "Kick", "iem": true})).unwrap();
        c.channel(2, &json!({"name": "Snare", "iem": true}))
            .unwrap();
        if o.pair {
            c.channel(3, &json!({"name": "Keys", "iem": true, "link": true}))
                .unwrap();
        } else {
            c.channel(3, &json!({"name": "Keys", "iem": true})).unwrap();
        }
        assert_eq!(c.iem_mix_add("Drums", None).unwrap(), "drums");
        if !o.pair {
            // The mix's talkback input on channel 4.
            let tb = c.state()["iem"]["mixes"][0]["talkback_input"]
                .as_u64()
                .unwrap();
            c.channel(4, &json!({"source": tb, "name": "Drummer TB"}))
                .unwrap();
        }
        c.iem_mix(&json!({"id": "drums", "master_db": 0.0, "channels": {
            "1": {"level_db": -6.0, "pan": -1.0}, "2": {"pan": 1.0}, "3": {}}}))
            .unwrap();
        // Its API: the flow's description and the push channel, as the desk program
        // serves them (behind site tokens with `tokens`).
        let gate = o
            .tokens
            .then(|| Arc::new(PasswordGate::tokens_only(check, "idn_mixer", "X-IDN-Mixer")));
        let handler: Handler = {
            let c = c.clone();
            Arc::new(move |r: Request| {
                let who = match &gate {
                    Some(g) => match g.who(&r) {
                        Ok(w) => w,
                        Err(resp) => return resp,
                    },
                    None => web::Who::local(&r),
                };
                match (r.method.as_str(), r.path.as_str()) {
                    ("GET", "/api/v1/iem") => Response::json(c.iem_description().to_string()),
                    ("GET", "/api/v1/events") => {
                        let c = c.clone();
                        Response::websocket(&r, move |ws| c.events_as(ws, who))
                    }
                    _ => Response::json_err("404 Not Found", "no such page"),
                }
            })
        };
        let in_site = (o.tls && o.site).then(|| InSite::new(&dir));
        let (tls, cert) = if let Some(s) = &in_site {
            let m = s._member.clone();
            let (cfg, fp) =
                web::lan::server_config(&dir, "idn-mixer", Arc::new(move || Some(m.clone())))
                    .unwrap();
            let der = std::fs::read(dir.join("panel-p256.crt")).unwrap();
            (Some(cfg), Some((der, fp)))
        } else if o.tls {
            let (cfg, fp) = web::self_signed(&dir, "idn-mixer").unwrap();
            let der = std::fs::read(dir.join("panel-p256.crt")).unwrap();
            (Some(cfg), Some((der, fp)))
        } else {
            (None, None)
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        web::serve(listener, tls, handler, stop.clone()).unwrap();
        Desk {
            c,
            port,
            stop,
            feeding,
            _dir: dir,
            key,
            site,
            cert,
            in_site,
        }
    }

    fn token(&self, sub: &str, scope: &str) -> String {
        let c = Claims::new(
            &self.site,
            sub,
            sub,
            "desk",
            "m1",
            Scope::parse(scope).unwrap(),
            idn_common::epoch::secs(),
            600,
        )
        .unwrap();
        token::sign(&c, &self.key).unwrap()
    }

    fn config(&self) -> Config {
        Config {
            desk: format!("127.0.0.1:{}", self.port),
            mix: Some("drums".into()),
            transport: Transport::Unicast,
            ..Config::default()
        }
    }

    /// The desk's mix "drums" now.
    fn mix(&self) -> Value {
        let s = self.c.state();
        s["iem"]["mixes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == "drums")
            .unwrap()
            .clone()
    }

    /// The desk's UDP port for phones.
    fn udp(&self) -> u16 {
        self.c.iem_description()["unicast"]["port"]
            .as_u64()
            .unwrap() as u16
    }
}

/// An audio output in the test: renders 5 ms at a time, at real-time pace, keeping the
/// last 10 s.
struct Player {
    out: Arc<Mutex<Vec<f32>>>,
    stop: Arc<AtomicBool>,
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn play(rx: &Receiver) -> Player {
    let out = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let (rx, o, s) = (rx.clone(), out.clone(), stop.clone());
    std::thread::spawn(move || {
        let start = Instant::now();
        let mut buf = vec![0f32; 480];
        let mut k = 0u64;
        while !s.load(Ordering::Relaxed) {
            rx.render(&mut buf);
            {
                let mut o = o.lock_ok();
                o.extend_from_slice(&buf);
                let keep = 2 * 48_000 * 10;
                if o.len() > keep + 48_000 {
                    let cut = o.len() - keep;
                    o.drain(..cut);
                }
            }
            k += 1;
            let next = start + Duration::from_micros(5000 * k);
            std::thread::sleep(next.saturating_duration_since(Instant::now()));
        }
    });
    Player { out, stop }
}

impl Player {
    /// The last `frames` frames (stereo, interleaved).
    fn recent(&self, frames: usize) -> Vec<f32> {
        let o = self.out.lock_ok();
        o[o.len().saturating_sub(2 * frames)..].to_vec()
    }
}

/// The amplitude of `f` Hz on `side` (0 left, 1 right) over the last 0.2 s (whole cycles
/// of every test tone: no leakage between them).
fn amp(x: &[f32], side: usize, f: f32) -> f32 {
    let n = x.len() / 2;
    let (mut re, mut im) = (0.0f64, 0.0f64);
    for k in 0..n {
        let a = 2.0 * std::f64::consts::PI * f64::from(f) * k as f64 / 48_000.0;
        let v = f64::from(x[2 * k + side]);
        re += v * a.cos();
        im -= v * a.sin();
    }
    (2.0 * (re * re + im * im).sqrt() / n.max(1) as f64) as f32
}

fn db(x: f32) -> f32 {
    10f32.powf(x / 20.0)
}

/// Waits (up to 4 s) until each (side, tone, amplitude) holds: within 1.5 dB, or under
/// 0.01 for 0.
fn expect(p: &Player, what: &str, want: &[(usize, f32, f32)]) {
    let end = Instant::now() + Duration::from_secs(4);
    loop {
        let x = p.recent(9600);
        let got: Vec<f32> = want.iter().map(|&(s, f, _)| amp(&x, s, f)).collect();
        let ok = x.len() == 2 * 9600
            && want.iter().zip(&got).all(|(&(_, _, w), &g)| {
                if w == 0.0 {
                    g < 0.01
                } else {
                    (20.0 * (g / w).log10()).abs() < 1.5
                }
            });
        if ok {
            return;
        }
        assert!(Instant::now() < end, "{what}: want {want:?}, got {got:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// CLOCK_MONOTONIC, ns (the desk's own clock for the flow's times off a site).
fn mono_ns() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: fills a timespec of our own.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    #[allow(clippy::unnecessary_cast)]
    let ns = ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64;
    ns
}

/// From the desk's tap to the phone playing it (ms): the median and the largest of 40
/// looks over 2 s.
fn latency(rx: &Receiver) -> Option<(f64, f64)> {
    let mut v = Vec::new();
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(50));
        let (t, flags, at) = rx.last_frame()?;
        if flags & idn_iem::wire::TIME_SITE != 0 {
            return None;
        }
        let played = mono_ns() - at.elapsed().as_nanos() as i64;
        v.push((played - t as i64) as f64 / 1e6);
    }
    v.sort_by(f64::total_cmp);
    Some((v[v.len() / 2], v[v.len() - 1]))
}

/// The receiver's events so far.
fn events(rx: &Receiver) -> Vec<Event> {
    std::iter::from_fn(|| rx.next_event(Duration::ZERO)).collect()
}

/// Unicast on the desk's own machine: the mix heard at its levels and pans (a pair by its
/// balance); the phone's mute, solo, master and a drag reach the desk; the sound
/// person's change reaches the phone; the latency from the desk's tap to playing.
#[test]
fn a_phone_mixes_the_flow_and_follows_the_mix_both_ways() {
    let d = Desk::new(
        "mix",
        Opts {
            pair: true,
            ..Default::default()
        },
    );
    let rx = Receiver::connect(d.config()).unwrap();
    let chans = rx.channels();
    assert_eq!(
        chans.iter().map(|c| (c.id, c.pair)).collect::<Vec<_>>(),
        [(1, None), (2, None), (3, Some(4)), (4, Some(3))]
    );
    assert_eq!(chans[0].name, "Kick");
    let m = rx.mix().expect("the mix, after connecting");
    assert_eq!((m.id.as_str(), m.channels[&1].level_db), ("drums", -6.0));
    let p = play(&rx);
    let half = AMP * db(-6.0);
    expect(
        &p,
        "the mix as saved",
        &[
            (0, 500.0, half),
            (1, 500.0, 0.0),
            (0, 1500.0, 0.0),
            (1, 1500.0, AMP),
            (0, 2400.0, AMP),
            (1, 2400.0, 0.0),
            (0, 3600.0, 0.0),
            (1, 3600.0, AMP),
        ],
    );
    let st = rx.status();
    assert!(st.connected && st.push, "{st:?}");
    assert_eq!((st.transport, st.channels), (Transport::Unicast, 4));
    assert!((10.0..=100.0).contains(&st.target_ms), "{st:?}");
    if let Some((med, max)) = latency(&rx) {
        let st = rx.status();
        eprintln!(
            "latency, desk tap to playing: median {med:.1} ms, max {max:.1} ms \
             (buffer {:.1} ms, target {:.1} ms, rtt {:?} ms)",
            st.buffer_ms, st.target_ms, st.rtt_ms
        );
        // (About 20 ms on an idle machine; a loaded one schedules the desk's and the
        // player's threads late, which the buffer then covers.)
        assert!(med < st.buffer_ms as f64 + 30.0, "{med} ms");
    }
    // The phone: mute, solo, master.
    rx.set_mute(1, true).unwrap();
    expect(&p, "channel 1 muted", &[(0, 500.0, 0.0), (1, 1500.0, AMP)]);
    wait_for("the mute on the desk", Duration::from_secs(3), || {
        d.mix()["channels"]["1"]["mute"] == true
    });
    rx.set_mute(1, false).unwrap();
    rx.set_solo(2, true).unwrap();
    expect(
        &p,
        "channel 2 soloed",
        &[
            (0, 500.0, 0.0),
            (1, 1500.0, AMP),
            (0, 2400.0, 0.0),
            (1, 3600.0, 0.0),
        ],
    );
    rx.set_solo(2, false).unwrap();
    rx.set_master(-6.0).unwrap();
    expect(
        &p,
        "master −6",
        &[(0, 500.0, half * db(-6.0)), (1, 1500.0, half)],
    );
    wait_for("the master on the desk", Duration::from_secs(3), || {
        d.mix()["master_db"] == -6.0
    });
    assert_eq!(d.mix()["channels"]["2"]["solo"], false);
    // The sound person changes the musician's mix: heard on the phone.
    let _ = events(&rx);
    d.c.iem_mix(&json!({"id": "drums", "channels": {"2": {"level_db": -12.0}}}))
        .unwrap();
    wait_for(
        "the desk's change on the phone",
        Duration::from_secs(3),
        || rx.mix().unwrap().channels[&2].level_db == -12.0,
    );
    expect(&p, "the desk's change", &[(1, 1500.0, AMP * db(-18.0))]);
    assert!(events(&rx).contains(&Event::MixChanged));
    // The pair's balance, by its right side's id: its left side halved.
    rx.set_pan(4, 0.5).unwrap();
    expect(
        &p,
        "the pair's balance",
        &[
            (0, 2400.0, AMP * 0.5 * db(-6.0)),
            (1, 3600.0, AMP * db(-6.0)),
        ],
    );
    wait_for("the balance on the desk", Duration::from_secs(3), || {
        d.mix()["channels"]["3"]["pan"] == 0.5
    });
    // A drag: many changes quickly; the desk ends at the last, the phone too.
    for k in 0..60 {
        rx.set_level(1, -40.0 + k as f32 * 0.5).unwrap();
        std::thread::sleep(Duration::from_millis(3));
    }
    wait_for("the drag's end on the desk", Duration::from_secs(3), || {
        d.mix()["channels"]["1"]["level_db"] == -10.5
    });
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(rx.mix().unwrap().channels[&1].level_db, -10.5);
    // A channel out of the mix and back.
    rx.set_in_mix(2, false).unwrap();
    expect(&p, "channel 2 out", &[(1, 1500.0, 0.0)]);
    wait_for("channel 2 out on the desk", Duration::from_secs(3), || {
        d.mix()["channels"]["2"].is_null()
    });
    assert_eq!(rx.set_level(9, 0.0), Err(Error::UnknownChannel(9)));
    drop(p);
    rx.close();
    wait_for(
        "the session's end at the desk",
        Duration::from_secs(3),
        || {
            d.c.state()["iem"]["phones"]
                .as_array()
                .is_some_and(Vec::is_empty)
        },
    );
}

/// A desk on a machine in a site: the phone pins the site CA alone (as from the Site
/// Manager's `GET /api/v1/site_ca`) and reaches the desk by address; the desk's own
/// self-signed certificate is no longer what its network port presents.
#[test]
fn the_site_ca_pinned() {
    let d = Desk::new(
        "site",
        Opts {
            tls: true,
            site: true,
            ..Default::default()
        },
    );
    let ca = d.in_site.as_ref().unwrap().ca.clone();
    let rx = Receiver::connect(Config {
        trust: Some(Trust::Certificate(ca)),
        ..d.config()
    })
    .unwrap();
    assert!(rx.status().session);
    let p = play(&rx);
    expect(
        &p,
        "with the site CA pinned",
        &[(0, 500.0, AMP * db(-6.0)), (1, 1500.0, AMP)],
    );
    drop(p);
    drop(rx);
    let (own, _) = d.cert.clone().unwrap();
    let e = Receiver::connect(Config {
        trust: Some(Trust::Certificate(own)),
        ..d.config()
    })
    .err()
    .unwrap();
    assert!(matches!(e, Error::Tls(_)), "{e:?}");
}

/// The network's way: HTTPS with the desk's certificate pinned (or its fingerprint), a
/// musician's `iem:` token that names its mix, refusals as typed errors, a read token
/// that may listen but not talk.
#[test]
fn tokens_and_a_pinned_certificate() {
    let d = Desk::new(
        "tokens",
        Opts {
            tokens: true,
            tls: true,
            ..Default::default()
        },
    );
    let (der, fp) = d.cert.clone().unwrap();
    let alice = d.token("alice", "iem:drums");
    let base = Config {
        token: alice.clone(),
        trust: Some(Trust::Certificate(der.clone())),
        mix: None,
        talk: true,
        ..d.config()
    };
    let rx = Receiver::connect(base.clone()).unwrap();
    assert_eq!(rx.mix().unwrap().id, "drums", "the token's own mix");
    assert!(rx.status().talk_allowed);
    let p = play(&rx);
    expect(
        &p,
        "over the network's way",
        &[(0, 500.0, AMP * db(-6.0)), (1, 1500.0, AMP)],
    );
    rx.set_level(2, -3.0).unwrap();
    wait_for(
        "the musician's change on the desk",
        Duration::from_secs(3),
        || d.mix()["channels"]["2"]["level_db"] == -3.0,
    );
    drop(p);
    drop(rx);
    // The fingerprint does as well.
    let rx = Receiver::connect(Config {
        trust: Some(Trust::Sha256(fp)),
        ..base.clone()
    })
    .unwrap();
    assert!(rx.status().session);
    drop(rx);
    // Refusals.
    let other = Trust::Sha256("00".repeat(32));
    let e = Receiver::connect(Config {
        trust: Some(other),
        ..base.clone()
    })
    .err()
    .unwrap();
    assert!(matches!(e, Error::Tls(_)), "{e:?}");
    let e = Receiver::connect(Config {
        token: String::new(),
        ..base.clone()
    })
    .err()
    .unwrap();
    assert!(matches!(e, Error::TokenNeeded(_)), "{e:?}");
    let forged = {
        let k = KeyFile::generate().unwrap();
        let c = Claims::new(
            &d.site,
            "eve",
            "eve",
            "desk",
            "m1",
            Scope::parse("iem:drums").unwrap(),
            idn_common::epoch::secs(),
            600,
        )
        .unwrap();
        token::sign(&c, &k).unwrap()
    };
    let e = Receiver::connect(Config {
        token: forged,
        ..base.clone()
    })
    .err()
    .unwrap();
    assert!(matches!(e, Error::TokenRefused(_)), "{e:?}");
    // A read token: listens (its mix named), may not talk.
    let rx = Receiver::connect(Config {
        token: d.token("bob", "read"),
        mix: Some("drums".into()),
        ..base.clone()
    })
    .unwrap();
    let st = rx.status();
    assert!(st.session && !st.talk_allowed, "{st:?}");
    assert!(matches!(rx.talk_start(), Err(Error::NotAllowed(_))));
    assert!(
        events(&rx)
            .iter()
            .any(|e| matches!(e, Event::TalkRefused { .. })),
    );
    assert_eq!(rx.mix().unwrap().id, "drums");
    // Its changes are refused by the desk: the desk's value comes back.
    let before = rx.mix().unwrap().master_db;
    rx.set_master(-20.0).unwrap();
    wait_for("the refusal", Duration::from_secs(3), || {
        events(&rx)
            .iter()
            .any(|e| matches!(e, Event::Error(Error::Desk(_))))
    });
    wait_for("the desk's value back", Duration::from_secs(3), || {
        rx.mix().unwrap().master_db == before
    });
}

/// A relay between the phone and the desk that loses, reorders and repeats packets:
/// the sound stays continuous (concealed); then the path goes dark long enough for the
/// desk to forget the session, and the phone subscribes again (UNKNOWN).
#[test]
fn loss_reordering_and_a_dark_path() {
    let d = Desk::new(
        "lossy",
        Opts {
            timeout: Some(Duration::from_millis(1500)),
            ..Default::default()
        },
    );
    let desk: SocketAddr = ([127, 0, 0, 1], d.udp()).into();
    let relay = UdpSocket::bind("127.0.0.1:0").unwrap();
    relay
        .set_read_timeout(Some(Duration::from_millis(1)))
        .unwrap();
    let rport = relay.local_addr().unwrap().port();
    let dark = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let counts = Arc::new(Mutex::new((0u32, 0u32, 0u32)));
    {
        let (dark, stop, counts) = (dark.clone(), stop.clone(), counts.clone());
        std::thread::spawn(move || {
            let mut phone: Option<SocketAddr> = None;
            let mut held: Vec<(Instant, Vec<u8>)> = Vec::new();
            let mut seed = 0x2545_f491_4f6c_dd1du64;
            let mut rnd = move || {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                (seed % 1000) as u32
            };
            let mut buf = [0u8; 5000];
            while !stop.load(Ordering::Relaxed) {
                let now = Instant::now();
                if let Some(ph) = phone {
                    held.retain(|(t, b)| {
                        if *t <= now {
                            let _ = relay.send_to(b, ph);
                            false
                        } else {
                            true
                        }
                    });
                }
                let Ok((n, from)) = relay.recv_from(&mut buf) else {
                    continue;
                };
                if dark.load(Ordering::Relaxed) {
                    continue;
                }
                let b = buf[..n].to_vec();
                if from != desk {
                    phone = Some(from);
                    let _ = relay.send_to(&b, desk);
                    continue;
                }
                let Some(ph) = phone else { continue };
                let audio = b.get(3) == Some(&1);
                let r = rnd();
                let mut c = counts.lock_ok();
                if audio && r < 50 {
                    c.0 += 1; // lost (5%)
                } else if audio && r < 150 {
                    c.1 += 1; // late by 8–12 ms (10%): reordered
                    held.push((now + Duration::from_millis(8 + u64::from(r % 5)), b));
                } else {
                    let _ = relay.send_to(&b, ph);
                    if audio && r < 180 {
                        c.2 += 1; // twice (3%)
                        let _ = relay.send_to(&b, ph);
                    }
                }
            }
        });
    }
    let rx = Receiver::connect(Config {
        udp_port: Some(rport),
        ..d.config()
    })
    .unwrap();
    let p = play(&rx);
    expect(
        &p,
        "through the relay",
        &[(0, 500.0, AMP * db(-6.0)), (1, 1500.0, AMP)],
    );
    std::thread::sleep(Duration::from_secs(3));
    // Continuous: no 5 ms of the last 2.5 s far below the tone's level.
    let x = p.recent(120_000);
    let want = AMP * db(-6.0) / std::f32::consts::SQRT_2;
    let worst = x
        .as_chunks::<480>()
        .0
        .iter()
        .map(|w| (w.iter().step_by(2).map(|v| v * v).sum::<f32>() / 240.0).sqrt())
        .fold(f32::MAX, f32::min);
    let st = rx.status();
    let (lost, late, twice) = *counts.lock_ok();
    eprintln!(
        "relay: {lost} lost, {late} reordered, {twice} twice; phone: {:?}, loss {:.1}%, \
         buffer {:.1} ms (target {:.1}); quietest 5 ms {:.3} of {want:.3}",
        st.counters, st.loss_pct, st.buffer_ms, st.target_ms, worst
    );
    // The tone without a gap: in a release build (the full run's release half). A debug
    // build's unoptimized receiver, beside the rest of a full run, heard a 15 ms gap
    // twice (2026-11-19), passing alone every time; its counters are checked in both.
    if !cfg!(debug_assertions) {
        assert!(worst > want * 0.3, "a gap: {worst} of {want}");
    }
    let c = st.counters;
    assert!(c.duplicates > 0 && c.concealed > 0, "{c:?}");
    assert!(st.loss_pct > 0.5, "{st:?}");
    // Dark for 2.5 s: the desk forgets the session; the phone comes back by itself.
    let _ = events(&rx);
    let subs = rx.status().subscribes;
    dark.store(true, Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(2500));
    dark.store(false, Ordering::Relaxed);
    wait_for("sound again", Duration::from_secs(6), || {
        let st = rx.status();
        st.connected && st.subscribes > subs
    });
    expect(&p, "after the dark", &[(1, 1500.0, AMP)]);
    assert!(
        events(&rx)
            .iter()
            .any(|e| matches!(e, Event::Disconnected { .. })),
    );
    stop.store(true, Ordering::Relaxed);
}

/// Push-to-talk: a 44.1 kHz stereo microphone, made mono 48 kHz Opus, arrives on the
/// mix's talkback input (desk channel 4); silent once released.
#[test]
fn talkback_reaches_the_desk() {
    let d = Desk::new("talk", Opts::default());
    let rx = Receiver::connect(Config {
        talk: true,
        ..d.config()
    })
    .unwrap();
    assert!(rx.status().talk_allowed);
    let level = || d.c.meters()["ch"][3][0].as_f64().unwrap_or(-144.0);
    // (Channel 4 played input 4's tone until it took the talkback input: its meter falls.)
    wait_for(
        "silence on channel 4 before talking",
        Duration::from_secs(5),
        || level() < -60.0,
    );
    rx.talk_start().unwrap();
    let talker = {
        let rx = rx.clone();
        std::thread::spawn(move || {
            let start = Instant::now();
            for k in 0..150u64 {
                let chunk: Vec<f32> = (0..441)
                    .flat_map(|i| {
                        let t = (k as usize * 441 + i) as f32 / 44_100.0;
                        let v = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * t).sin();
                        [v, v]
                    })
                    .collect();
                rx.talk_feed(&chunk, 44_100, 2).unwrap();
                let next = start + Duration::from_millis(10 * (k + 1));
                std::thread::sleep(next.saturating_duration_since(Instant::now()));
            }
        })
    };
    wait_for("the talkback on channel 4", Duration::from_secs(5), || {
        level() > -20.0
    });
    assert_eq!(d.mix()["talking"], true);
    talker.join().unwrap();
    rx.talk_stop().unwrap();
    wait_for("silence after the button", Duration::from_secs(5), || {
        level() < -60.0
    });
    assert!(!rx.status().talking);
}

/// Multicast on this host, when its network carries it to itself (else said and
/// skipped); auto picks a way that works.
#[test]
fn multicast_and_auto() {
    let d = Desk::new("mcast", Opts::default());
    let rx = Receiver::connect(Config {
        transport: Transport::Multicast,
        ..d.config()
    })
    .unwrap();
    let got = wait_until(Duration::from_secs(3), || rx.status().connected);
    if got {
        assert_eq!(rx.status().transport, Transport::Multicast);
        let p = play(&rx);
        expect(&p, "by multicast", &[(1, 1500.0, AMP)]);
        let ph = &d.c.state()["iem"]["phones"][0];
        assert_eq!(
            (ph["multicast"].as_bool(), ph["unicast"].as_bool()),
            (Some(true), Some(false))
        );
    } else {
        eprintln!(
            "multicast: no audio by multicast on this host ({:?}); skipped",
            rx.status().error
        );
    }
    drop(rx);
    let rx = Receiver::connect(Config {
        transport: Transport::Auto,
        multicast_wait: Duration::from_millis(800),
        ..d.config()
    })
    .unwrap();
    let p = play(&rx);
    expect(&p, "auto", &[(1, 1500.0, AMP)]);
    let st = rx.status();
    eprintln!("auto: {:?}", st.transport);
    assert_eq!(st.asked, Transport::Auto);
    // The phone picks unicast now.
    rx.set_transport(Transport::Unicast).unwrap();
    wait_for("unicast at the desk", Duration::from_secs(4), || {
        d.c.state()["iem"]["phones"][0]["unicast"] == true
    });
    expect(&p, "unicast after", &[(1, 1500.0, AMP)]);
}

static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

extern "C" fn on_event(_user: *mut std::ffi::c_void, json: *const std::ffi::c_char) {
    // SAFETY: the library gives a NUL-terminated string for the call.
    let s = unsafe { std::ffi::CStr::from_ptr(json) }.to_str().unwrap();
    assert!(s.starts_with("{\"type\""), "{s}");
    CALLS.fetch_add(1, Ordering::Relaxed);
}

/// The C ABI from end to end: connect, render, change, read JSON, events, refusals.
#[test]
fn the_c_abi() {
    use idn_iem::ffi::*;
    use std::ffi::{CStr, CString};
    let d = Desk::new("cabi", Opts::default());
    let take = |p: *mut std::ffi::c_char| -> Value {
        assert!(!p.is_null());
        // SAFETY: from the library, freed once below.
        let v = serde_json::from_str(unsafe { CStr::from_ptr(p) }.to_str().unwrap()).unwrap();
        // SAFETY: as above.
        unsafe { idn_iem_string_free(p) };
        v
    };
    let cfg = CString::new(
        json!({"desk": format!("127.0.0.1:{}", d.port), "mix": "drums", "transport": "unicast"})
            .to_string(),
    )
    .unwrap();
    let mut code = 1;
    let mut err = [0 as std::ffi::c_char; 256];
    // SAFETY: valid arguments throughout.
    unsafe {
        let h = idn_iem_connect(
            cfg.as_ptr(),
            std::ptr::null(),
            0,
            &mut code,
            err.as_mut_ptr(),
            err.len(),
        );
        assert!(!h.is_null() && code == IDN_IEM_OK);
        let mut buf = vec![0f32; 2 * 240];
        let start = Instant::now();
        let mut heard = 0.0f32;
        for k in 0..300u64 {
            assert_eq!(idn_iem_render(h, buf.as_mut_ptr(), 240), IDN_IEM_OK);
            heard = heard.max(buf.iter().fold(0.0, |a, v| a.max(v.abs())));
            let next = start + Duration::from_micros(5000 * (k + 1));
            std::thread::sleep(next.saturating_duration_since(Instant::now()));
        }
        assert!(heard > 0.1, "{heard}");
        assert_eq!(idn_iem_set_level(h, 2, -9.0), IDN_IEM_OK);
        assert_eq!(idn_iem_set_mute(h, 1, 1), IDN_IEM_OK);
        assert_eq!(idn_iem_set_master(h, -1.0), IDN_IEM_OK);
        assert_eq!(idn_iem_set_level(h, 40, 0.0), IDN_IEM_UNKNOWN_CHANNEL);
        assert_eq!(idn_iem_set_level(h, 2, f32::NAN), IDN_IEM_CONFIG);
        let m = take(idn_iem_mix_json(h));
        assert_eq!(
            (m["id"].as_str(), m["channels"]["2"]["level_db"].as_f64()),
            (Some("drums"), Some(-9.0))
        );
        wait_for(
            "the C side's change on the desk",
            Duration::from_secs(3),
            || {
                d.mix()["channels"]["2"]["level_db"] == -9.0
                    && d.mix()["channels"]["1"]["mute"] == true
            },
        );
        let ch = take(idn_iem_channels_json(h));
        assert_eq!(ch.as_array().map(Vec::len), Some(3));
        let st = take(idn_iem_status_json(h));
        assert_eq!(
            (st["session"].as_bool(), st["transport"].as_str()),
            (Some(true), Some("unicast"))
        );
        let ev = take(idn_iem_next_event(h, 1000));
        assert!(ev["type"].is_string(), "{ev}");
        assert_eq!(idn_iem_talk_start(h), IDN_IEM_NOT_ALLOWED);
        assert_eq!(idn_iem_set_transport(h, 7), IDN_IEM_BAD_ARGUMENT);
        assert_eq!(idn_iem_set_output_latency_ms(h, 12.0), IDN_IEM_OK);
        assert_eq!(take(idn_iem_status_json(h))["output_ms"], 12.0);
        assert_eq!(
            idn_iem_set_event_callback(h, Some(on_event), std::ptr::null_mut()),
            IDN_IEM_OK
        );
        d.c.iem_mix(&json!({"id": "drums", "master_db": -2.0}))
            .unwrap();
        wait_for(
            "an event through the callback",
            Duration::from_secs(3),
            || CALLS.load(Ordering::Relaxed) > 0,
        );
        assert_eq!(
            idn_iem_set_event_callback(h, None, std::ptr::null_mut()),
            IDN_IEM_OK
        );
        assert!(
            !CStr::from_ptr(idn_iem_version())
                .to_str()
                .unwrap()
                .is_empty()
        );
        idn_iem_free(h);
        // Refused: no desk there.
        let none = UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let cfg = CString::new(json!({"desk": format!("127.0.0.1:{none}")}).to_string()).unwrap();
        let h = idn_iem_connect(
            cfg.as_ptr(),
            std::ptr::null(),
            0,
            &mut code,
            err.as_mut_ptr(),
            err.len(),
        );
        assert!(h.is_null());
        assert_eq!(code, IDN_IEM_IO, "{:?}", CStr::from_ptr(err.as_ptr()));
        let bad = CString::new("{").unwrap();
        assert!(
            idn_iem_connect(
                bad.as_ptr(),
                std::ptr::null(),
                0,
                &mut code,
                std::ptr::null_mut(),
                0
            )
            .is_null()
        );
        assert_eq!(code, IDN_IEM_CONFIG);
        assert_eq!(
            idn_iem_render(std::ptr::null(), buf.as_mut_ptr(), 1),
            IDN_IEM_BAD_ARGUMENT
        );
    }
}

/// What decoding and mixing 24 channels costs (run with `--release -- --ignored`): the
/// share of one core the audio thread spends.
#[test]
#[ignore]
fn the_cost_of_24_channels() {
    use idn_iem::codec::{Decoder, Encoder};
    let frames = 2000; // 10 s
    let mut data = Vec::new();
    let mut enc = Encoder::new(240, 64_000).unwrap();
    for k in 0..frames {
        let pcm: Vec<f32> = (0..240)
            .map(|i| {
                0.3 * (2.0 * std::f32::consts::PI * 440.0 * (k * 240 + i) as f32 / 48_000.0).sin()
            })
            .collect();
        data.push(enc.encode(&pcm).unwrap().to_vec());
    }
    let mut decs: Vec<Decoder> = (0..24).map(|_| Decoder::new().unwrap()).collect();
    let mut out = vec![0f32; 480];
    let t = Instant::now();
    for f in &data {
        out.fill(0.0);
        for d in &mut decs {
            let pcm = d.decode(Some(f), 240).unwrap();
            for (k, x) in pcm.iter().enumerate() {
                out[2 * k] += x * 0.5;
                out[2 * k + 1] += x * 0.5;
            }
        }
    }
    let share = t.elapsed().as_secs_f64() / 10.0;
    eprintln!(
        "24 channels decoded and mixed: {:.1} % of a core",
        share * 100.0
    );
}
