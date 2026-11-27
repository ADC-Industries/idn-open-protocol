//! The control and file channel (`channel/1`, spec/extensions/idn-extensions.md §E.12)
//! between devices enrolled with a real authority and Discovery Server.

use crate::common::{Site, device, site};
use idn_common::sync::LockOk;
use idn_common::testing::{wait_for, wait_until};
use idn_open_protocol::channel::{
    Channel, ChannelConfig, FileInfo, FileSink, Handler, PathSink, Peer, Reply, Served,
};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Waited this long for a message, a file or a change in the directory.
const WAIT: Duration = Duration::from_secs(10);

/// Records what arrives, serves `files`, accepts pushes of names ending in `.ok`.
#[derive(Default)]
struct App {
    seen: Mutex<Vec<String>>,
    files: Mutex<Vec<(String, PathBuf)>>,
    inbox: PathBuf,
}

impl Handler for App {
    fn on_message(&self, from: &Peer, kind: &str, payload: Vec<u8>) {
        self.seen.lock_ok().push(format!(
            "{} {kind} {}",
            from.name,
            String::from_utf8_lossy(&payload)
        ));
    }
    fn on_request(&self, from: &Peer, kind: &str, payload: Vec<u8>) -> Reply {
        match kind {
            "test.echo" => Reply::Ok([from.kind.as_bytes(), b":", &payload].concat()),
            "test.fail" => Reply::Error("it broke".into()),
            "test.deny" => Reply::Refused,
            _ => Reply::Unknown,
        }
    }
    fn file(&self, _: &Peer, id: &str) -> Option<PathBuf> {
        let f = self.files.lock_ok();
        f.iter().find(|(i, _)| i == id).map(|(_, p)| p.clone())
    }
    fn on_push(&self, _: &Peer, offer: &FileInfo) -> Option<Box<dyn FileSink>> {
        offer
            .name
            .ends_with(".ok")
            .then(|| Box::new(PathSink::new(self.inbox.join(&offer.name))) as Box<dyn FileSink>)
    }
    fn on_received(&self, _: &Peer, offer: &FileInfo, result: io::Result<()>) {
        self.seen
            .lock_ok()
            .push(format!("received {} {}", offer.name, result.is_ok()));
    }
    fn on_served(&self, to: &Peer, id: &str, served: Served) {
        let what = match served {
            Served::Progress { .. } => return,
            Served::Done { size } => format!("done {size}"),
            Served::Failed(e) => format!("failed {e}"),
        };
        self.seen
            .lock_ok()
            .push(format!("served {id} to {} {what}", to.name));
    }
}

/// An app whose pushes land in `{tag}-inbox` in the site's folder.
fn app(site: &Site, tag: &str) -> Arc<App> {
    Arc::new(App {
        inbox: site.dir.join(format!("{tag}-inbox")),
        ..Default::default()
    })
}

/// Waits until `ch` lists the device `fp` in its directory.
fn sees(ch: &Channel, fp: &str) {
    let ok = wait_until(WAIT, || ch.devices().iter().any(|d| d.fingerprint == fp));
    assert!(ok, "{fp} not in the directory: {:?}", ch.devices());
}

#[test]
fn messages_requests_and_status_between_enrolled_devices() {
    let site = site("chan-m", Some("hall"));
    let (ea, eb) = (site.member("ma (App)"), site.member("mb (App)"));
    let (aa, ab) = (app(&site, "ma"), app(&site, "mb"));
    let a = Channel::start(ChannelConfig::new(ea.clone()), aa.clone()).unwrap();
    let b = Channel::start(ChannelConfig::new(eb.clone()), ab.clone()).unwrap();
    sees(&a, &eb.fingerprint());
    sees(&b, &ea.fingerprint());
    let d = a
        .devices()
        .into_iter()
        .find(|d| d.fingerprint == eb.fingerprint())
        .unwrap();
    assert_eq!((d.name.as_str(), d.kind.as_str()), ("mb (App)", "app"));

    // A one-way message, with the sender's identity.
    a.send(&eb.fingerprint(), "stage.message", b"Hello")
        .unwrap();
    wait_for("B's app hears A's message", WAIT, || {
        ab.seen
            .lock_ok()
            .iter()
            .any(|s| s == "ma (App) stage.message Hello")
    });

    // Requests and each status.
    let t = Duration::from_secs(5);
    let fp = eb.fingerprint();
    assert_eq!(
        a.request(&fp, "test.echo", b"x", t).unwrap(),
        Reply::Ok(b"app:x".to_vec())
    );
    assert_eq!(
        a.request(&fp, "test.fail", b"", t).unwrap(),
        Reply::Error("it broke".into())
    );
    assert_eq!(a.request(&fp, "test.deny", b"", t).unwrap(), Reply::Refused);
    assert_eq!(
        a.request(&fp, "test.other", b"", t).unwrap(),
        Reply::Unknown
    );
    assert!(
        a.request(&fp, "Bad Type", b"", t).is_err(),
        "checked before sending"
    );

    // Status, asked over the channel.
    b.set_status(
        "Example Player",
        "Playing (Welcome10-21-26.mp4)",
        &[("slide", "3")],
    );
    let Reply::Ok(s) = a.request(&fp, "idn.status", b"", t).unwrap() else {
        panic!()
    };
    let s = String::from_utf8(s).unwrap();
    assert!(
        s.contains(r#"app="Example Player""#) && s.contains("Welcome10-21-26.mp4"),
        "{s}"
    );
    assert!(s.contains(r#"name="slide" value="3""#), "{s}");

    // And on the site's monitoring panel (§E.12.6).
    let monitor = idn_open_protocol::discovery::Monitor::start(&site.server).unwrap();
    assert!(
        wait_until(WAIT, || monitor.devices().iter().any(|d| d.name
            == "mb (App)"
            && d.get("x-idn-app") == Some("Example Player")
            && d.get("x-idn-activity")
                == Some("Playing (Welcome10-21-26.mp4)")
            && d.get("x-idn-status-slide") == Some("3"))),
        "{:#?}",
        monitor.devices()
    );
}

/// A sink that fails after `limit` bytes, like a transfer cut off midway.
struct CutAfter {
    inner: PathSink,
    left: usize,
}

impl FileSink for CutAfter {
    fn held(&mut self) -> io::Result<Option<(u64, String)>> {
        self.inner.held()
    }
    fn restart(&mut self) -> io::Result<()> {
        self.inner.restart()
    }
    fn begin(&mut self, info: &FileInfo) -> io::Result<()> {
        self.inner.begin(info)
    }
    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        let n = data.len().min(self.left);
        self.inner.write(&data[..n])?;
        self.left -= n;
        if self.left == 0 {
            return Err(io::Error::other("cut"));
        }
        Ok(())
    }
    fn finish(&mut self, info: &FileInfo) -> io::Result<()> {
        self.inner.finish(info)
    }
}

#[test]
fn files_resume_after_an_interruption_and_restart_when_changed() {
    let site = site("chan-f", Some("hall"));
    let (ea, eb) = (site.member("fa (App)"), site.member("fb (App)"));
    let (aa, ab) = (app(&site, "fa"), app(&site, "fb"));
    let a = Channel::start(ChannelConfig::new(ea.clone()), aa).unwrap();
    let _b = Channel::start(ChannelConfig::new(eb.clone()), ab.clone()).unwrap();
    sees(&a, &eb.fingerprint());

    let src_dir = site.dir.join("f-src");
    std::fs::create_dir_all(&src_dir).unwrap();
    let src = src_dir.join("service.mp4");
    let data: Vec<u8> = (0..20_000_000u32)
        .map(|i| i.wrapping_mul(2_654_435_761) as u8)
        .collect();
    std::fs::write(&src, &data).unwrap();
    ab.files.lock_ok().push(("video-1".into(), src.clone()));
    let dest = site.dir.join("f-dest").join("service.mp4");

    // Cut off after 5 MB...
    let mut cut = CutAfter {
        inner: PathSink::new(&dest),
        left: 5_000_000,
    };
    assert!(
        a.fetch(&eb.fingerprint(), "video-1", &mut cut, &mut |_, _| {})
            .is_err()
    );
    let mut sink = PathSink::new(&dest);
    assert_eq!(sink.held().unwrap().map(|(n, _)| n), Some(5_000_000));
    // ...then resumed from there: only the rest travels.
    let mut first = None;
    let info = a
        .fetch(&eb.fingerprint(), "video-1", &mut sink, &mut |have, _| {
            first.get_or_insert(have);
        })
        .unwrap();
    assert!(first.unwrap() > 5_000_000, "resumed, not restarted");
    assert_eq!(info.size, data.len() as u64);
    assert_eq!(std::fs::read(&dest).unwrap(), data);

    // A partial copy of a file that then changes: the transfer starts over (status 4).
    let dest2 = site.dir.join("f-dest2").join("service.mp4");
    let mut cut = CutAfter {
        inner: PathSink::new(&dest2),
        left: 1_000_000,
    };
    assert!(
        a.fetch(&eb.fingerprint(), "video-1", &mut cut, &mut |_, _| {})
            .is_err()
    );
    let changed: Vec<u8> = data.iter().map(|b| b ^ 0x5a).collect();
    std::fs::write(&src, &changed).unwrap();
    a.fetch(
        &eb.fingerprint(),
        "video-1",
        &mut PathSink::new(&dest2),
        &mut |_, _| {},
    )
    .unwrap();
    assert_eq!(std::fs::read(&dest2).unwrap(), changed);

    // No such file.
    let e = a
        .fetch(
            &eb.fingerprint(),
            "nothing",
            &mut PathSink::new(site.dir.join("f-none").join("x")),
            &mut |_, _| {},
        )
        .unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::NotFound);
}

#[test]
fn pushes_are_accepted_or_refused_by_the_receiving_app() {
    let site = site("chan-p", Some("hall"));
    let (ea, eb) = (site.member("pa (App)"), site.member("pb (App)"));
    let (aa, ab) = (app(&site, "pa"), app(&site, "pb"));
    let a = Channel::start(ChannelConfig::new(ea.clone()), aa.clone()).unwrap();
    let _b = Channel::start(ChannelConfig::new(eb.clone()), ab.clone()).unwrap();
    sees(&a, &eb.fingerprint());
    let dir = site.dir.join("p-src");
    std::fs::create_dir_all(&dir).unwrap();
    let (ok, no) = (dir.join("slides.ok"), dir.join("slides.no"));
    std::fs::write(&ok, b"the slides").unwrap();
    std::fs::write(&no, b"other").unwrap();
    let t = Duration::from_secs(5);
    assert_eq!(
        a.push(&eb.fingerprint(), "s1", &ok, t).unwrap(),
        Reply::Ok(Vec::new())
    );
    assert_eq!(
        a.push(&eb.fingerprint(), "s2", &no, t).unwrap(),
        Reply::Refused
    );
    wait_for("the pushed slides received", WAIT, || {
        ab.seen
            .lock_ok()
            .iter()
            .any(|s| s == "received slides.ok true")
    });
    assert_eq!(
        std::fs::read(ab.inbox.join("slides.ok")).unwrap(),
        b"the slides"
    );
    // The offering side hears the whole file was taken (read to its end, acknowledged;
    // a sender's "sent", 2026-11-26).
    assert!(
        wait_until(WAIT, || aa
            .seen
            .lock_ok()
            .iter()
            .any(|s| s.starts_with("served s1 to ") && s.ends_with(" done 10"))),
        "{:?}",
        aa.seen.lock_ok()
    );
    assert!(!aa.seen.lock_ok().iter().any(|s| s.contains("failed")));
}

#[test]
fn no_channel_without_site_trust_and_revocation_cuts_off() {
    let site = site("chan-r", Some("hall"));
    // Not enrolled (no passphrase, no approval): no channel.
    let outsider = device(&site.server, &site.dir.join("rx"), "rx (App)", None);
    let e = Channel::start(ChannelConfig::new(outsider), app(&site, "rx"))
        .err()
        .unwrap();
    assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);

    let (ea, eb) = (site.member("ra (App)"), site.member("rb (App)"));
    let a = Channel::start(ChannelConfig::new(ea.clone()), app(&site, "ra")).unwrap();
    let _b = Channel::start(ChannelConfig::new(eb.clone()), app(&site, "rb")).unwrap();
    sees(&a, &eb.fingerprint());
    let t = Duration::from_secs(3);
    assert_eq!(
        a.request(&eb.fingerprint(), "test.echo", b"", t).unwrap(),
        Reply::Ok(b"app:".to_vec())
    );
    // Revoke B: it leaves the directory, and A can't reach it any more.
    site.authority.revoke(&eb.fingerprint()).unwrap();
    wait_for("B gone from A's directory", WAIT, || {
        !a.devices()
            .iter()
            .any(|d| d.fingerprint == eb.fingerprint())
    });
    assert!(a.request(&eb.fingerprint(), "test.echo", b"", t).is_err());
}

#[test]
fn a_speed_cap_set_while_running_applies() {
    use idn_open_protocol::channel::TroubleResponse;
    let site = site("chan-c", Some("hall"));
    let (ea, eb) = (site.member("ca (App)"), site.member("cb (App)"));
    let (aa, ab) = (app(&site, "ca"), app(&site, "cb"));
    let a = Channel::start(ChannelConfig::new(ea.clone()), aa).unwrap();
    let b = Channel::start(ChannelConfig::new(eb.clone()), ab.clone()).unwrap();
    sees(&a, &eb.fingerprint());
    assert_eq!(
        b.trouble_response(),
        TroubleResponse::PerDevice,
        "the default"
    );
    b.set_trouble_response(TroubleResponse::Off);
    assert_eq!(b.trouble_response(), TroubleResponse::Off);

    let dir = site.dir.join("c-src");
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("clip.mp4");
    std::fs::write(&src, vec![7u8; 6_000_000]).unwrap();
    ab.files.lock_ok().push(("clip".into(), src));
    let fetch = |n: u32| {
        let t = Instant::now();
        let dest = site.dir.join(format!("c-dest{n}")).join("clip.mp4");
        a.fetch(
            &eb.fingerprint(),
            "clip",
            &mut PathSink::new(dest),
            &mut |_, _| {},
        )
        .unwrap();
        t.elapsed().as_secs_f64()
    };
    // The holder caps what it sends: 6 MB at 3 MB/s takes about 2 s...
    b.set_max_rate(Some(3_000_000));
    assert_eq!(b.max_rate(), Some(3_000_000));
    let capped = fetch(1);
    assert!((1.6..3.0).contains(&capped), "{capped} s with a 3 MB/s cap");
    // ...and much less without the cap.
    b.set_max_rate(None);
    let free = fetch(2);
    assert!(
        free < capped / 2.0,
        "{free} s without the cap, {capped} s with it"
    );
}

/// Answers only `test.main`: a machine's announced endpoint (its IDN Node's).
struct MainOnly;
impl Handler for MainOnly {
    fn on_request(&self, _: &Peer, kind: &str, _: Vec<u8>) -> Reply {
        match kind {
            "test.main" => Reply::Ok(b"main".to_vec()),
            _ => Reply::Unknown,
        }
    }
}

/// A device with two endpoints (its announced one and a program's that only reaches
/// out, §E.12.3): the other device keeps a connection to each,
/// so one connecting never cuts the other's transfer; a request to the device reaches
/// whichever endpoint knows its type; a push's file is fetched back from the endpoint
/// that offered it.
#[test]
fn a_device_with_two_endpoints() {
    let site = site("chan-two", Some("hall"));
    let (ea, eb) = (site.member("twa (App)"), site.member("twb (App)"));
    let aa = app(&site, "twa");
    let a = Channel::start(ChannelConfig::new(ea.clone()), aa.clone()).unwrap();
    let main = Channel::start(ChannelConfig::new(eb.clone()), Arc::new(MainOnly)).unwrap();
    let pb = app(&site, "twb");
    let prog = Channel::start(
        ChannelConfig {
            client_only: true,
            ..ChannelConfig::new(eb.clone())
        },
        pb.clone(),
    )
    .unwrap();
    let (fa, fb) = (ea.fingerprint(), eb.fingerprint());
    sees(&a, &fb);
    sees(&prog, &fa);
    sees(&main, &fa);
    let t = Duration::from_secs(5);

    // A slow transfer A → the program (8 MB at 4 MB/s), while B's announced endpoint
    // connects to A: the program's connection stays (it was replaced, and the transfer
    // cut, before).
    let big = site.dir.join("two-big");
    std::fs::create_dir_all(&big).unwrap();
    let file = big.join("song.wav");
    std::fs::write(&file, vec![7u8; 8 << 20]).unwrap();
    aa.files.lock_ok().push(("song".into(), file.clone()));
    a.set_max_rate(Some(4_000_000));
    let (prog2, fa2, to) = (prog.clone(), fa.clone(), site.dir.join("two-got"));
    let got = to.join("song.wav");
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let s2 = started.clone();
    let fetch = std::thread::spawn(move || {
        let mut sink = PathSink::new(&got);
        prog2.fetch(&fa2, "song", &mut sink, &mut |done, _| {
            if done > 0 {
                s2.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        })
    });
    wait_for("the slow transfer's start", WAIT, || {
        started.load(std::sync::atomic::Ordering::Relaxed)
    });
    assert_eq!(
        main.request(&fa, "test.echo", b"m", t).unwrap(),
        Reply::Ok(b"app:m".to_vec())
    );
    let info = fetch
        .join()
        .unwrap()
        .expect("not cut by the other endpoint");
    assert_eq!(info.size, 8 << 20);
    a.set_max_rate(None);

    // A's requests to B: the announced endpoint answers its own type, the program
    // its; neither knows it: unknown.
    assert_eq!(
        a.request(&fb, "test.main", b"", t).unwrap(),
        Reply::Ok(b"main".to_vec())
    );
    assert_eq!(
        a.request(&fb, "test.echo", b"p", t).unwrap(),
        Reply::Ok(b"app:p".to_vec())
    );
    assert_eq!(a.request(&fb, "test.none", b"", t).unwrap(), Reply::Unknown);
    // A message reaches the program too.
    a.send(&fb, "stage.message", b"hi").unwrap();
    wait_for("the program hears A's message", WAIT, || {
        pb.seen
            .lock_ok()
            .iter()
            .any(|s| s == "twa (App) stage.message hi")
    });

    // The program pushes a file only it offers: A fetches it back from the program.
    let note = big.join("note.ok");
    std::fs::write(&note, b"from the program").unwrap();
    assert_eq!(
        prog.push(&fa, "note", &note, t).unwrap(),
        Reply::Ok(Vec::new())
    );
    wait_for("the program's note received", WAIT, || {
        aa.seen
            .lock_ok()
            .iter()
            .any(|s| s == "received note.ok true")
    });
    assert_eq!(
        std::fs::read(aa.inbox.join("note.ok")).unwrap(),
        b"from the program"
    );
}
