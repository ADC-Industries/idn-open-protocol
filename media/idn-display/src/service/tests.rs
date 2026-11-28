//! The display service without screens or a network: a driver in memory stands for the
//! screens' processes (what they were sent, what they say).

use super::*;
use crate::proto::{Showing, Stats};
use idn_open_protocol::trust::pki::KeyFile;
use idn_open_protocol::trust::token::{self, Claims, Scope};
use idn_open_protocol::web::{Request, SiteKeys, TokenCheck};

/// What the fake screens were sent, and what they are to say next, by screen.
#[derive(Default)]
struct Shared {
    sent: BTreeMap<String, Vec<ToChild>>,
    say: BTreeMap<String, Vec<FromChild>>,
    started: BTreeMap<String, usize>,
    end: BTreeMap<String, String>,
}

struct Fake {
    screens: Mutex<Vec<ScreenInfo>>,
    shared: Arc<Mutex<Shared>>,
}

struct FakeLink {
    name: String,
    shared: Arc<Mutex<Shared>>,
}

impl Driver for Fake {
    fn list(&self) -> Result<Vec<ScreenInfo>, String> {
        Ok(self.screens.lock_ok().clone())
    }

    fn start(&self, s: &ScreenInfo) -> Result<Box<dyn ChildLink>, String> {
        let mut g = self.shared.lock_ok();
        *g.started.entry(s.name.clone()).or_default() += 1;
        g.end.remove(&s.name);
        g.say
            .entry(s.name.clone())
            .or_default()
            .push(FromChild::Ready {
                screen: s.clone(),
                adapter: "test GPU".into(),
            });
        Ok(Box::new(FakeLink {
            name: s.name.clone(),
            shared: self.shared.clone(),
        }))
    }
}

impl ChildLink for FakeLink {
    fn send(&mut self, m: &ToChild) -> Result<(), String> {
        let mut g = self.shared.lock_ok();
        // A thumbnail is answered at once, with a JPEG of the screen's colour.
        if let ToChild::Thumbnail { id, width } = m {
            let h = width * 9 / 16;
            let rgba: Vec<u8> = (0..width * h).flat_map(|_| [20u8, 200, 40, 255]).collect();
            let jpeg = idn_common::jpeg::encode_rgba(*width as usize, h as usize, &rgba, 80);
            g.say
                .entry(self.name.clone())
                .or_default()
                .push(FromChild::Thumbnail {
                    id: *id,
                    jpeg: idn_common::text::base64(jpeg),
                    error: None,
                });
        }
        g.sent.entry(self.name.clone()).or_default().push(m.clone());
        Ok(())
    }

    fn recv(&mut self) -> Vec<FromChild> {
        std::mem::take(
            self.shared
                .lock_ok()
                .say
                .entry(self.name.clone())
                .or_default(),
        )
    }

    fn ended(&mut self) -> Option<String> {
        self.shared.lock_ok().end.get(&self.name).cloned()
    }
}

fn screen(name: &str, x: i32) -> ScreenInfo {
    ScreenInfo {
        name: name.into(),
        width: 1920,
        height: 1080,
        refresh_hz: 60.0,
        x,
        y: 0,
        modes: vec![
            crate::proto::ScreenMode {
                width: 1920,
                height: 1080,
                refresh_hz: 60.0,
            },
            crate::proto::ScreenMode {
                width: 1280,
                height: 720,
                refresh_hz: 60.0,
            },
        ],
    }
}

fn dir(name: &str) -> idn_common::testing::TempDir {
    idn_common::testing::TempDir::new(&format!("display-svc-{name}"))
}

/// A service on two fake screens, and its site's key and id.
fn service(d: &Path) -> (Arc<Service>, Arc<Mutex<Shared>>, KeyFile, String) {
    let key = KeyFile::generate().unwrap();
    let site = key.fingerprint()[..16].to_owned();
    let (public, s2) = (key.public, site.clone());
    let check = TokenCheck::new("display", move || {
        Some(SiteKeys {
            ca_public: public,
            site: s2.clone(),
            machine: "m1".into(),
            options: Default::default(),
        })
    });
    let shared = Arc::new(Mutex::new(Shared::default()));
    let fake = Fake {
        screens: Mutex::new(vec![screen("DP-1", 0), screen("HDMI-A-1", 1920)]),
        shared: shared.clone(),
    };
    let svc = Service::new(d, Some(d.join("media")), Box::new(fake), Some(check));
    (svc, shared, key, site)
}

fn request(method: &str, path: &str, query: &str, auth: Option<&str>, body: Value) -> Request {
    let mut headers: std::collections::HashMap<String, String> =
        [("x-idn-display".to_owned(), "1".to_owned())].into();
    if let Some(t) = auth {
        headers.insert("authorization".into(), format!("Bearer {t}"));
    }
    Request {
        method: method.into(),
        path: path.into(),
        query: query.into(),
        headers,
        body: if body.is_null() {
            Vec::new()
        } else {
            body.to_string().into_bytes()
        },
        peer: "127.0.0.1".parse().unwrap(),
        tls: false,
    }
}

fn post(svc: &Arc<Service>, cmd: &str, body: Value) -> Response {
    svc.handle(request("POST", &format!("/api/v1/{cmd}"), "", None, body))
}

use idn_open_protocol::web::Response;

fn body(r: &Response) -> Value {
    serde_json::from_slice(&r.body).unwrap_or(Value::Null)
}

/// A feed set: the screen's process started, told its settings and then its feed; a new
/// feed goes to the same process (no restart, no gap); no feed: the process stops.
#[test]
fn a_feed_change_goes_to_the_running_screen() {
    let d = dir("feed");
    let (svc, shared, _, _) = service(&d);
    svc.tend();
    // Nothing set: no process (the desktop stays on the screens).
    assert!(shared.lock_ok().started.is_empty());
    let r = post(
        &svc,
        "feed",
        json!({"screen": 1, "source": "BOOTH (Program)"}),
    );
    assert_eq!(r.status, "200 OK", "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(body(&r)["screen"], "DP-1");
    svc.tend();
    {
        let g = shared.lock_ok();
        assert_eq!(g.started.get("DP-1"), Some(&1));
        let sent = &g.sent["DP-1"];
        assert!(matches!(sent[0], ToChild::Settings { .. }), "{sent:?}");
        assert_eq!(
            sent[1],
            ToChild::Feed {
                source: "BOOTH (Program)".into()
            }
        );
    }
    // Another feed: the same process, told the new feed.
    let r = post(
        &svc,
        "feed",
        json!({"screen": "DP-1", "source": "LOBBY (Feed)"}),
    );
    assert_eq!(r.status, "200 OK");
    svc.tend();
    {
        let g = shared.lock_ok();
        assert_eq!(g.started.get("DP-1"), Some(&1), "not started again");
        assert_eq!(
            g.sent["DP-1"].last(),
            Some(&ToChild::Feed {
                source: "LOBBY (Feed)".into()
            })
        );
    }
    let st = svc.state();
    assert_eq!(st["screens"][0]["feed"], "LOBBY (Feed)");
    assert_eq!(st["screens"][0]["running"], true);
    assert_eq!(st["screens"][0]["adapter"], "test GPU");
    assert_eq!(st["screens"][1]["feed"], "");
    assert_eq!(st["screens"][1]["number"], 2);
    // A screen that isn't there, and a bad name.
    assert!(
        post(&svc, "feed", json!({"screen": 3, "source": "X"}))
            .status
            .starts_with("400")
    );
    assert!(
        post(&svc, "feed", json!({"screen": "DP-1", "source": "A\nB"}))
            .status
            .starts_with("400")
    );
    // No feed: the screen's process stops, the desktop is back.
    post(&svc, "feed", json!({"screen": "DP-1", "source": ""}));
    svc.tend();
    assert_eq!(svc.state()["screens"][0]["running"], false);
    // Its process ended by itself: started again after 3 s, and the reason said.
    post(&svc, "feed", json!({"screen": "DP-1", "source": "A (B)"}));
    svc.tend();
    shared
        .lock_ok()
        .end
        .insert("DP-1".into(), "no GPU for the display".into());
    svc.tend();
    let st = svc.state();
    assert_eq!(st["screens"][0]["error"], "no GPU for the display");
    let (evs, _) = svc.events_since(0);
    assert!(evs.iter().any(|e| e["event"] == "stopped"), "{evs:?}");
    svc.tend();
    assert_eq!(shared.lock_ok().started.get("DP-1"), Some(&2), "waits 3 s");
}

/// Settings are checked, only allowed files taken, kept per screen by name, and read back
/// by a new service (a restart).
#[test]
fn settings_are_checked_and_kept() {
    let d = dir("settings");
    std::fs::create_dir_all(d.join("media/slides")).unwrap();
    let logo = idn_common::jpeg::encode_rgba(8, 8, &[255u8; 8 * 8 * 4], 80);
    std::fs::write(d.join("media/church.jpg"), &logo).unwrap();
    let elsewhere = dir("outside");
    let outside = elsewhere.join("logo.png");
    std::fs::write(&outside, &logo).unwrap();
    let (svc, shared, _, _) = service(&d);
    svc.tend();
    let bad = |b: Value| {
        let r = post(&svc, "settings", b);
        assert!(
            r.status.starts_with("400"),
            "{}",
            String::from_utf8_lossy(&r.body)
        );
        body(&r)["error"].as_str().unwrap_or_default().to_owned()
    };
    bad(json!({"screen": "DP-1", "rotation": 45}));
    bad(json!({"screen": "DP-1", "overscan": 30}));
    bad(json!({"screen": "DP-1", "fit": "zoom"}));
    bad(json!({"screen": "DP-1", "audio": {"volume_db": 40}}));
    bad(json!({"screen": "DP-1", "fallback": {"after_s": -2}}));
    bad(json!({"screen": "DP-1", "wall": {"rows": 2, "cols": 2, "row": 2, "col": 0}}));
    bad(
        json!({"screen": "DP-1", "wall": {"rows": 1, "cols": 2, "row": 0, "col": 0, "bezel_mm": {"h": 9, "v": 0}}}),
    );
    // A file outside the media and state folders can't be a logo (it could be read back
    // as a thumbnail).
    let e = bad(
        json!({"screen": "DP-1", "fallback": {"show": "logo", "logo": outside.display().to_string()}}),
    );
    assert!(e.contains("media folder"), "{e}");
    bad(json!({"screen": "DP-1", "fallback": {"show": "slideshow", "slideshow": "nowhere"}}));
    // Taken: one field at a time, the rest kept.
    let ok = |b: Value| {
        let r = post(&svc, "settings", b);
        assert_eq!(r.status, "200 OK", "{}", String::from_utf8_lossy(&r.body));
    };
    ok(json!({"screen": "DP-1", "rotation": 90, "fit": "fill"}));
    ok(json!({"screen": "DP-1", "overscan": 2.5}));
    ok(json!({"screen": "DP-1", "audio": {"device": "", "channels": [2, 3], "volume_db": -6}}));
    ok(
        json!({"screen": "DP-1", "fallback": {"after_s": 3, "show": "logo", "logo": "church.jpg", "reconnect_cap": 5}}),
    );
    ok(json!({"screen": "HDMI-A-1", "fallback": {"show": "slideshow", "slideshow": "slides"}}));
    ok(
        json!({"screen": 2, "wall": {"rows": 1, "cols": 2, "row": 0, "col": 1,
        "bezel_mm": {"h": 12, "v": 0}, "screen_mm": {"w": 1210, "h": 680}}}),
    );
    let st = svc.state();
    let s0 = &st["screens"][0]["settings"];
    assert_eq!(
        (s0["rotation"].as_u64(), s0["fit"].as_str()),
        (Some(90), Some("fill"))
    );
    assert_eq!(s0["overscan"], 2.5);
    assert_eq!(s0["audio"]["channels"], json!([2, 3]));
    assert_eq!(s0["fallback"]["reconnect_cap"], 5);
    assert_eq!(st["screens"][1]["settings"]["wall"]["col"], 1);
    // The screen's process gets the files as full paths.
    post(
        &svc,
        "feed",
        json!({"screen": "DP-1", "source": "BOOTH (Program)"}),
    );
    svc.tend();
    {
        let g = shared.lock_ok();
        let Some(ToChild::Settings { settings }) = g.sent["DP-1"]
            .iter()
            .find(|m| matches!(m, ToChild::Settings { .. }))
        else {
            panic!()
        };
        assert!(std::path::Path::new(&settings.fallback.logo).is_absolute());
        assert!(settings.fallback.logo.ends_with("church.jpg"));
    }
    // Wall null: out of the wall.
    ok(json!({"screen": 2, "wall": null}));
    assert!(svc.state()["screens"][1]["settings"]["wall"].is_null());
    // A restart: everything back from screens.json.
    drop(svc);
    let (svc2, _, _, _) = service(&d);
    svc2.tend();
    let st = svc2.state();
    assert_eq!(st["screens"][0]["feed"], "BOOTH (Program)");
    assert_eq!(st["screens"][0]["settings"]["rotation"], 90);
    assert_eq!(
        st["screens"][0]["settings"]["fallback"]["logo"],
        "church.jpg"
    );
    assert_eq!(
        st["screens"][1]["settings"]["fallback"]["show"],
        "slideshow"
    );
    // A screen unplugged keeps its settings (listed as not connected), until forgotten.
    let r = post(&svc2, "forget", json!({"screen": "HDMI-A-1"}));
    assert_eq!(r.status, "200 OK");
}

/// A damaged `screens.json` (a power cut mid-write) is set aside, not read as no screens
/// and then saved over at the first change: the person's settings kept to put back.
#[test]
fn a_damaged_screens_file_is_set_aside() {
    let d = dir("damaged");
    std::fs::write(d.join(FILE), b"{\"DP-1\": {\"feed\": \"BOOTH").unwrap();
    let (svc, _, _, _) = service(&d);
    svc.tend();
    assert_eq!(svc.state()["screens"][0]["feed"], "");
    let aside: Vec<String> = std::fs::read_dir(&*d)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("screens.json.bad-"))
        .collect();
    assert_eq!(aside.len(), 1, "set aside");
    assert_eq!(
        std::fs::read(d.join(&aside[0])).unwrap(),
        b"{\"DP-1\": {\"feed\": \"BOOTH"
    );
    // A change saves a new file; the one set aside stays as it was.
    let r = post(
        &svc,
        "feed",
        json!({"screen": "DP-1", "source": "BOOTH (Program)"}),
    );
    assert_eq!(r.status, "200 OK");
    assert!(d.join(FILE).exists() && d.join(&aside[0]).exists());
}

/// The site's tokens: a read-only one reads and is refused changes (in the audit), a
/// display token changes, another engine's is refused; on this machine, open.
#[test]
fn tokens_and_the_audit() {
    let d = dir("tokens");
    let (svc, _, key, site) = service(&d);
    svc.tend();
    let make = |sub: &str, engine: &str, scope: Scope| {
        let c = Claims::new(
            &site,
            sub,
            sub,
            engine,
            "*",
            scope,
            idn_common::epoch::secs(),
            600,
        )
        .unwrap();
        token::sign(&c, &key).unwrap()
    };
    let read = make("alice", "display", Scope::Read);
    let control = make("bob", "display", Scope::Control);
    let desk = make("bob", "desk", Scope::Control);
    let lan = |method: &str, path: &str, tok: Option<&str>, b: Value| {
        let mut r = request(method, path, "", tok, b);
        r.peer = "10.0.0.9".parse().unwrap();
        r.tls = true;
        svc.handle_lan(r)
    };
    let feed = json!({"screen": "DP-1", "source": "BOOTH (Program)"});
    // No token on the network: not logged in.
    assert!(
        lan("GET", "/api/v1/state", None, Value::Null)
            .status
            .starts_with("403")
    );
    let r = lan("GET", "/api/v1/state", Some(&read), Value::Null);
    assert_eq!(r.status, "200 OK");
    let r = lan("POST", "/api/v1/feed", Some(&read), feed.clone());
    assert!(r.status.starts_with("403"), "{}", r.status);
    assert_eq!(svc.state()["screens"][0]["feed"], "");
    let r = lan("POST", "/api/v1/feed", Some(&desk), feed.clone());
    assert!(r.status.starts_with("401"), "{}", r.status);
    let r = lan("POST", "/api/v1/feed", Some(&control), feed.clone());
    assert_eq!(r.status, "200 OK", "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(svc.state()["screens"][0]["feed"], "BOOTH (Program)");
    // This machine: open, written as local.
    let r = post(&svc, "identify", json!({"screen": "all", "seconds": 2}));
    assert_eq!(r.status, "200 OK");
    // Without the change header (a cross-site form): refused.
    let mut r = request("POST", "/api/v1/feed", "", None, feed.clone());
    r.headers.remove("x-idn-display");
    assert!(svc.handle(r).status.starts_with("403"));
    svc.audit.flush();
    let log = svc.audit.newest(10);
    assert!(
        log.iter()
            .any(|l| l["user"] == "alice" && l["ok"] == false && l["action"] == "feed"),
        "{log:?}"
    );
    assert!(log.iter().any(|l| l["user"] == "bob"
        && l["ok"] == true
        && l["peer"] == "10.0.0.9"
        && l["detail"]["source"] == "BOOTH (Program)"));
    assert!(
        log.iter()
            .any(|l| l["user"] == "local" && l["action"] == "identify")
    );
    let r = svc.handle(request("GET", "/api/v1/audit", "n=5", None, Value::Null));
    assert_eq!(r.status, "200 OK");
    let r = svc.handle(request("GET", "/api/v1/schema", "", None, Value::Null));
    assert!(String::from_utf8_lossy(&r.body).contains("reconnect_cap"));
}

/// The thumbnail: what the screen's process draws, as a JPEG; none for a screen with no
/// feed.
#[test]
fn the_thumbnail_is_a_jpeg() {
    let d = dir("thumb");
    let (svc, _, _, _) = service(&d);
    svc.tend();
    let r = svc.handle(request(
        "GET",
        "/api/v1/thumbnail",
        "screen=1",
        None,
        Value::Null,
    ));
    assert!(r.status.starts_with("404"), "no feed: {}", r.status);
    post(
        &svc,
        "feed",
        json!({"screen": 1, "source": "BOOTH (Program)"}),
    );
    svc.tend();
    svc.tend();
    // The service's own thread answers while this waits.
    let tender = svc.start();
    let r = svc.handle(request(
        "GET",
        "/api/v1/thumbnail",
        "screen=DP-1&width=320",
        None,
        Value::Null,
    ));
    svc.stop();
    let _ = tender.join();
    assert_eq!(r.status, "200 OK", "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.ctype, "image/jpeg");
    assert_eq!(&r.body[..2], &[0xff, 0xd8]);
    assert_eq!(&r.body[r.body.len() - 2..], &[0xff, 0xd9]);
}

/// The screen's status as it moves: playing, lost (reconnecting, attempt by attempt),
/// the fallback, recovered; a new picture size; each a typed event, and the state.
#[test]
fn status_changes_make_typed_events() {
    let d = dir("events");
    let (svc, shared, _, _) = service(&d);
    svc.tend();
    post(
        &svc,
        "feed",
        json!({"screen": 1, "source": "BOOTH (Program)"}),
    );
    svc.tend();
    let seq0 = svc.event_seq();
    // The Site Manager's log (§N.41): the failures, each once.
    let logged = Arc::new(Mutex::new(Vec::<String>::new()));
    {
        let l = logged.clone();
        svc.log.set_sink(move |x| l.lock_ok().push(x.event.clone()));
    }
    let say = |st: Status| {
        shared
            .lock_ok()
            .say
            .entry("DP-1".into())
            .or_default()
            .push(FromChild::Status { status: st });
        svc.tend();
    };
    let playing = |w, h| Status {
        state: State::Playing,
        showing: Showing::Feed,
        stats: Stats {
            source_size: Some([w, h]),
            frame_rate: Some(30.0),
            ..Default::default()
        },
        ..Default::default()
    };
    say(playing(1920, 1080));
    let lost = |attempts| Status {
        state: State::Lost,
        showing: Showing::Last,
        reason: "the source went away".into(),
        attempts,
        ..playing(1920, 1080)
    };
    let v0 = svc.version();
    say(lost(0));
    assert!(svc.version() > v0, "the state moved");
    say(lost(1));
    say(lost(2));
    // A problem in the state while lost (§N.41).
    let p = &svc.state()["problems"][0];
    assert_eq!(
        (p["problem"].as_str(), p["screen"].as_str()),
        (Some("lost"), Some("DP-1"))
    );
    assert_eq!(p["attempts"], 2);
    say(Status {
        state: State::Fallback,
        showing: Showing::Logo,
        for_s: 4.0,
        ..lost(3)
    });
    say(Status {
        for_s: 9.0,
        ..playing(1920, 1080)
    });
    say(playing(1280, 720));
    // Decoder trouble: pictures failing to decode, decoding off the GPU.
    let decoding = |decode: &str, errors| Status {
        stats: Stats {
            decode: decode.into(),
            decode_errors: errors,
            ..playing(1280, 720).stats
        },
        ..playing(1280, 720)
    };
    say(decoding("gpu", 0));
    say(decoding("gpu", 4));
    say(decoding("cpu", 4));
    let (evs, _) = svc.events_since(seq0);
    let names: Vec<&str> = evs.iter().filter_map(|e| e["event"].as_str()).collect();
    assert_eq!(
        names,
        [
            "playing",
            "lost",
            "reconnecting",
            "reconnecting",
            "fallback",
            "reconnecting",
            "recovered",
            "format",
            "decoder_trouble",
            "decoder_trouble"
        ],
        "{evs:?}"
    );
    assert!(
        evs.iter()
            .all(|e| e["screen"] == "DP-1" && e["type"] == "event")
    );
    let fb = evs.iter().find(|e| e["event"] == "fallback").unwrap();
    assert_eq!(fb["show"], "logo");
    let rc: Vec<u64> = evs
        .iter()
        .filter(|e| e["event"] == "reconnecting")
        .filter_map(|e| e["attempt"].as_u64())
        .collect();
    assert_eq!(rc, [1, 2, 3]);
    let fm = evs.iter().find(|e| e["event"] == "format").unwrap();
    assert_eq!(fm["to"]["size"], json!([1280, 720]));
    assert!(
        evs.iter()
            .all(|e| e["level"].is_string() && e["text"].is_string())
    );
    assert_eq!(*logged.lock_ok(), ["lost", "fallback", "recovered"]);
    assert!(svc.state()["problems"].as_array().unwrap().is_empty());
    // A feed of another site: `other_site` beside `lost`.
    say(Status {
        reason: "BOOTH (Program) is enrolled with another site authority".into(),
        ..lost(0)
    });
    let (evs, _) = svc.events_since(seq0);
    let n = evs.len();
    assert_eq!(evs[n - 2]["event"], "lost");
    assert_eq!(evs[n - 2]["cause"], "other_site");
    assert_eq!(evs[n - 1]["event"], "other_site");
    say(playing(1280, 720));
    let st = svc.state();
    assert_eq!(st["screens"][0]["status"]["state"], "playing");
    assert_eq!(st["screens"][0]["stats"]["source_size"], json!([1280, 720]));
    // A routing source followed (§1.8).
    say(Status {
        routed_to: Some("CAM-2 (Camera)".into()),
        ..playing(1280, 720)
    });
    let (evs, _) = svc.events_since(seq0);
    assert_eq!(evs.last().unwrap()["event"], "routed");
    assert_eq!(svc.state()["screens"][0]["routed_to"], "CAM-2 (Camera)");
}

/// Identify: on a screen with no feed its process runs for the time asked, then stops.
#[test]
fn identify_shows_on_screens_without_a_feed_for_a_while() {
    let d = dir("identify");
    let (svc, shared, _, _) = service(&d);
    svc.tend();
    let r = post(
        &svc,
        "identify",
        json!({"screen": "HDMI-A-1", "seconds": 0.5}),
    );
    assert_eq!(r.status, "200 OK");
    svc.tend();
    svc.tend();
    {
        let g = shared.lock_ok();
        assert_eq!(g.started.get("HDMI-A-1"), Some(&1));
        let Some(ToChild::Identify { lines, .. }) = g.sent["HDMI-A-1"]
            .iter()
            .find(|m| matches!(m, ToChild::Identify { .. }))
        else {
            panic!("{:?}", g.sent)
        };
        assert_eq!(lines[..2], ["2".to_owned(), "HDMI-A-1".to_owned()]);
    }
    assert_eq!(svc.state()["screens"][1]["identify"], true);
    std::thread::sleep(Duration::from_millis(600));
    svc.tend();
    let st = svc.state();
    assert_eq!(st["screens"][1]["identify"], false);
    assert_eq!(st["screens"][1]["running"], false);
    assert!(
        post(&svc, "identify", json!({"screen": 1, "seconds": 0}))
            .status
            .starts_with("400")
    );
}

/// The artboard's display items (§D.8.7): a label (on identify, with the machine), who set
/// the feed, a mode from the screen's list, idle on the fallback, the test pattern for a
/// while, restart, the network API turned off, the size and drop problems.
#[test]
fn labels_modes_idle_test_pattern_restart_and_lan() {
    let d = dir("artboard");
    let (svc, shared, _, _) = service(&d);
    svc.tend();
    // A label, shown on identify above the screen's own name, and the machine's line.
    assert_eq!(
        post(&svc, "label", json!({"screen": 1, "label": "Stage left"})).status,
        "200 OK"
    );
    assert!(
        post(&svc, "label", json!({"screen": 1, "label": "x".repeat(61)}))
            .status
            .starts_with("400")
    );
    post(&svc, "identify", json!({"screen": 1, "seconds": 2}));
    svc.tend();
    svc.tend();
    {
        let g = shared.lock_ok();
        let Some(ToChild::Identify { lines, .. }) = g.sent["DP-1"]
            .iter()
            .find(|m| matches!(m, ToChild::Identify { .. }))
        else {
            panic!("{:?}", g.sent)
        };
        assert_eq!(
            lines[..3],
            ["1".to_owned(), "Stage left".into(), "DP-1".into()]
        );
        assert!(lines[3].starts_with(&idn_open_protocol::discovery::hostname()));
    }
    // A wall's tile: its place and the wall's group too (who mounts it sees where).
    assert_eq!(
        post(
            &svc,
            "settings",
            json!({"screen": 1, "wall": {"rows": 2, "cols": 3, "row": 0, "col": 1, "group": "front"}})
        )
        .status,
        "200 OK"
    );
    shared.lock_ok().sent.clear();
    post(&svc, "identify", json!({"screen": 1, "seconds": 2}));
    svc.tend();
    svc.tend();
    {
        let g = shared.lock_ok();
        let Some(ToChild::Identify { lines, .. }) = g.sent["DP-1"]
            .iter()
            .find(|m| matches!(m, ToChild::Identify { .. }))
        else {
            panic!("{:?}", g.sent)
        };
        assert!(
            lines.contains(&"Wall R1 C2 of 2×3 · front".to_owned()),
            "{lines:?}"
        );
    }
    post(&svc, "settings", json!({"screen": 1, "wall": null}));
    // Who set the feed: this machine's API is `local`.
    post(
        &svc,
        "feed",
        json!({"screen": 1, "source": "BOOTH (Program)"}),
    );
    let st = svc.state();
    assert_eq!(st["screens"][0]["label"], "Stage left");
    assert_eq!(st["screens"][0]["feed_by"], "local");
    assert!(st["screens"][0]["feed_at"].as_u64().unwrap() > 0);
    assert_eq!(st["screens"][0]["modes"][1]["width"], 1280);
    // A mode the screen offers, and one it doesn't.
    assert_eq!(
        post(
            &svc,
            "settings",
            json!({"screen": 1, "mode": {"width": 1280, "height": 720, "refresh_hz": 60}})
        )
        .status,
        "200 OK"
    );
    assert!(
        post(
            &svc,
            "settings",
            json!({"screen": 1, "mode": {"width": 1024, "height": 768}})
        )
        .status
        .starts_with("400")
    );
    assert_eq!(svc.state()["screens"][0]["settings"]["mode"]["width"], 1280);
    post(&svc, "settings", json!({"screen": 1, "mode": null}));
    assert!(svc.state()["screens"][0]["settings"]["mode"].is_null());
    // Idle on the fallback: the screen with no feed runs (its logo), not the desktop.
    post(&svc, "settings", json!({"screen": 2, "idle": "fallback"}));
    svc.tend();
    assert_eq!(shared.lock_ok().started.get("HDMI-A-1"), Some(&1));
    // The test pattern for a second, sent to the screen and taken off after.
    post(
        &svc,
        "test_pattern",
        json!({"screen": 2, "on": true, "seconds": 1}),
    );
    svc.tend();
    assert_eq!(svc.state()["screens"][1]["test_pattern"], true);
    std::thread::sleep(Duration::from_millis(1100));
    svc.tend();
    assert_eq!(svc.state()["screens"][1]["test_pattern"], false);
    {
        let g = shared.lock_ok();
        let tests: Vec<bool> = g.sent["HDMI-A-1"]
            .iter()
            .filter_map(|m| match m {
                ToChild::TestPattern { on } => Some(*on),
                _ => None,
            })
            .collect();
        assert_eq!(tests, [true, false]);
    }
    // Restart: the process started again, told its feed again.
    let started = shared.lock_ok().started.get("DP-1").copied();
    let seq0 = svc.event_seq();
    assert_eq!(post(&svc, "restart", json!({"screen": 1})).status, "200 OK");
    svc.tend();
    assert_eq!(
        shared.lock_ok().started.get("DP-1").copied(),
        started.map(|n| n + 1)
    );
    assert!(
        svc.events_since(seq0)
            .0
            .iter()
            .any(|e| e["event"] == "restarted")
    );
    // The network API off: refused there, kept, on again from here.
    assert_eq!(svc.state()["lan"]["on"], true);
    post(&svc, "lan", json!({"on": false}));
    let mut r = request("GET", "/api/v1/state", "", None, Value::Null);
    r.peer = "192.0.2.9".parse().unwrap();
    assert!(svc.handle_lan(r).status.starts_with("503"));
    assert!(d.join(SERVICE_FILE).exists());
    post(&svc, "lan", json!({"on": true}));
    assert!(!svc.lan_off());
}

#[test]
fn a_smaller_screen_and_dropped_pictures_are_problems() {
    let s = json!({"id": "DP-1", "width": 1280, "height": 720,
        "status": {"state": "playing"}, "settings": {"rotation": 0},
        "stats": {"source_size": [1920, 1080], "screen_size": [1280, 720]},
        "dropped_last_min": 75});
    let p = size_and_drop_problems(&s);
    let kinds: Vec<&str> = p.iter().filter_map(|x| x["problem"].as_str()).collect();
    assert_eq!(kinds, ["screen_smaller", "dropping"]);
    // A wall's screen shows a part: no problem; a screen as big: none.
    let mut w = s.clone();
    w["settings"]["wall"] = json!({"rows": 2, "cols": 2, "row": 0, "col": 0});
    w["dropped_last_min"] = json!(0);
    assert!(size_and_drop_problems(&w).is_empty());
    let mut big = s.clone();
    big["stats"]["screen_size"] = json!([1920, 1080]);
    big["dropped_last_min"] = json!(3);
    assert!(size_and_drop_problems(&big).is_empty());
}

/// A screen whose process keeps stopping before its window is up waits 3 s, then twice as
/// long each time up to a minute, and 3 s again once one came up; with no screens listed,
/// the listing waits 10 s, then twice as long each time up to 30 s, and 10 s again once
/// screens are there or a command asks for a listing soon.
#[test]
fn failing_screens_and_empty_listings_wait_longer_each_time() {
    let mut g = Inner::default();
    let now = Instant::now();
    let waits: Vec<u64> = (0..7)
        .map(|_| {
            g.failed_now("HDMI-1", "it stopped".into(), now);
            g.restart_wait("HDMI-1").as_secs()
        })
        .collect();
    assert_eq!(waits, [3, 6, 12, 24, 48, 60, 60]);
    assert_eq!(g.restart_wait("HDMI-2"), RESTART_AFTER);
    g.came_up("HDMI-1");
    assert_eq!(g.restart_wait("HDMI-1"), RESTART_AFTER);
    assert!(!g.failed.contains_key("HDMI-1"));
    assert_eq!(g.list_wait(), LIST_EVERY);
    let waits: Vec<u64> = (0..6)
        .map(|_| {
            g.listed(false);
            g.list_wait().as_secs()
        })
        .collect();
    assert_eq!(waits, [10, 20, 30, 30, 30, 30]);
    g.listed(true);
    assert_eq!(g.list_wait(), LIST_EVERY);
    g.listed(false);
    g.listed(false);
    g.listed_at = Some(Instant::now());
    g.list_soon();
    assert_eq!(g.list_wait(), LIST_EVERY);
    assert!(g.listed_at.is_none());
}

/// A screen whose process stopped waits before it is started again; a change to its
/// settings or feed starts it at the next look instead.
#[test]
fn a_setting_change_ends_the_restart_wait() {
    let d = dir("restart-wait");
    let (svc, shared, _, _) = service(&d);
    svc.tend();
    post(
        &svc,
        "feed",
        json!({"screen": 1, "source": "BOOTH (Program)"}),
    );
    svc.tend();
    assert_eq!(shared.lock_ok().started.get("DP-1"), Some(&1));
    shared
        .lock_ok()
        .end
        .insert("DP-1".into(), "it stopped".into());
    svc.tend();
    svc.tend();
    // (Within its wait: not started again yet.)
    assert_eq!(shared.lock_ok().started.get("DP-1"), Some(&1));
    let r = post(&svc, "settings", json!({"screen": 1, "rotation": 90}));
    assert_eq!(r.status, "200 OK", "{}", String::from_utf8_lossy(&r.body));
    svc.tend();
    assert_eq!(shared.lock_ok().started.get("DP-1"), Some(&2));
    shared
        .lock_ok()
        .end
        .insert("DP-1".into(), "it stopped".into());
    svc.tend();
    post(
        &svc,
        "feed",
        json!({"screen": 1, "source": "BOOTH (Preview)"}),
    );
    svc.tend();
    assert_eq!(shared.lock_ok().started.get("DP-1"), Some(&3));
}
