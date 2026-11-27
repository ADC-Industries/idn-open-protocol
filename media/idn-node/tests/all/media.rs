//! Media files between machines (idn-node.md §N.25, R.22 stage 3): two Nodes of a site;
//! a file of A's media folder sent to B lands in B's media folder, checked; a file
//! outside the media folder is refused.

use crate::common;
use common::free_port;

use idn_common::testing::{TempDir, wait_for};
use idn_open_protocol::discovery::DiscoveryServer;
use idn_open_protocol::node::Node;
use idn_open_protocol::trust::{Authority, AuthorityConfig, AuthorityEvent};
use idn_open_protocol::xml::Element;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

fn sock(name: &str) -> String {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/tmp".into());
    format!("{dir}/imedia-{}-{name}.sock", std::process::id())
}

/// The Node process and its state folder, stopped and removed however the test ends.
struct Kill(Child, String, #[allow(dead_code)] TempDir); // (the folder: held, gone with it)
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_file(&self.1);
    }
}

/// A Node of this test, joined to the site at `server` and approved.
fn node(tag: &str, server: &str, authority: &Authority) -> (Node, Kill) {
    node_as(tag, server, authority, false)
}

/// [`node`], joined as one of the site's administrators (`admin`: with the site's admin
/// code).
fn node_as(tag: &str, server: &str, authority: &Authority, admin: bool) -> (Node, Kill) {
    let endpoint = sock(tag);
    let state = TempDir::new(tag);
    let child = Command::new(env!("CARGO_BIN_EXE_idn-node"))
        .env("IDN_CLOCK", "off")
        .args(["run", "--user", "--state"])
        .arg(state.path())
        .args([
            "--endpoint",
            &endpoint,
            "--panel-port",
            &free_port().to_string(),
        ])
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let kill = Kill(child, endpoint.clone(), state);
    wait_for("the Node to start", Duration::from_secs(10), || {
        Node::connect_to(&endpoint, tag).is_ok()
    });
    let n = Node::connect_to(&endpoint, tag).unwrap();
    let mut set = Element::new("set")
        .with_attr("name", tag)
        .with_attr("server", server);
    if admin {
        set = set.with_attr(
            "admin_code",
            &authority.admin_code().expect("an admin code"),
        );
    }
    n.request(set).unwrap();
    let fp = n.enrollment().fingerprint();
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(AuthorityEvent::JoinRequested(r)) = authority.recv(Duration::from_millis(200))
            && r.fingerprint == fp
        {
            authority.approve(r.id).unwrap();
            break;
        }
        assert!(Instant::now() < end, "no join request from {tag}");
    }
    // This machine's person confirms the code too (§N.11).
    // (No confirming on the machine since 2026-11-18, §N.11.4: approved is in.)
    while n.enrollment().creds().is_none() {
        assert!(Instant::now() < end, "the Node isn't in the site");
        std::thread::sleep(Duration::from_millis(100));
    }
    (n, kill)
}

#[test]
fn a_media_file_goes_to_another_machine_of_the_site() {
    let auth_dir = TempDir::new("auth");
    let authority = Authority::open(AuthorityConfig {
        state_dir: auth_dir.to_path_buf(),
        passphrase: None,
    })
    .unwrap();
    let ds = DiscoveryServer::start(0, Some(authority.clone())).unwrap();
    let server = format!("127.0.0.1:{}", ds.port());
    authority.open_pairing(Duration::from_secs(120));
    let (a, _ka) = node("a", &server, &authority);
    let (b, _kb) = node("b", &server, &authority);
    let b_fp = b.enrollment().fingerprint();
    // A sees B as a machine to send to.
    let end = Instant::now() + Duration::from_secs(20);
    let media_a = loop {
        let m = a.request(Element::new("media")).unwrap();
        if m.children_named("machine")
            .any(|x| x.attr("fp") == Some(&b_fp))
        {
            break m;
        }
        assert!(
            Instant::now() < end,
            "B not in A's machines: {}",
            m.to_xml()
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    let dir_a = PathBuf::from(media_a.attr("dir").unwrap());
    std::fs::create_dir_all(&dir_a).unwrap();
    let body: Vec<u8> = (0..3_000_000u32).map(|i| (i * 7 % 251) as u8).collect();
    std::fs::write(dir_a.join("Welcome video.mp4"), &body).unwrap();
    // A file outside the media folder is refused.
    let outside = TempDir::new("outside");
    std::fs::write(outside.join("secret.txt"), b"no").unwrap();
    for e in [
        Element::new("media_send")
            .with_attr("to", &b_fp)
            .with_attr("path", &outside.join("secret.txt").to_string_lossy()),
        Element::new("media_send")
            .with_attr("to", &b_fp)
            .with_attr("file", "../../outside/secret.txt"),
    ] {
        let err = a.request(e).unwrap_err().to_string();
        assert!(err.contains("only a file of the media folder"), "{err}");
    }
    // Sent by name: B's Node accepts and fetches it into its media folder.
    let r = a
        .request(
            Element::new("media_send")
                .with_attr("to", "b")
                .with_attr("file", "Welcome video.mp4"),
        )
        .unwrap();
    assert_eq!(r.name, "ok", "{}", r.to_xml());
    let end = Instant::now() + Duration::from_secs(30);
    let dir_b = loop {
        let m = b.request(Element::new("media")).unwrap();
        let done = m
            .children_named("transfer")
            .find(|t| t.attr("way") == Some("in"));
        if let Some(t) = done {
            assert_eq!(t.attr("peer"), Some("a"));
            if t.attr("state") == Some("received") {
                break PathBuf::from(m.attr("dir").unwrap());
            }
            assert!(
                !t.attr("state").unwrap_or("").starts_with("failed"),
                "{}",
                m.to_xml()
            );
        }
        assert!(Instant::now() < end, "not received: {}", m.to_xml());
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(
        std::fs::read(dir_b.join("Welcome video.mp4")).unwrap(),
        body
    );
    // A says B fetched all of it: "sent" (2026-11-26).
    wait_sent(&a, r.attr("offer").expect("the offer's id"), body.len());
    // Again: kept beside it, not over it.
    let r = a
        .request(
            Element::new("media_send")
                .with_attr("to", &b_fp)
                .with_attr("file", "Welcome video.mp4"),
        )
        .unwrap();
    assert_eq!(r.name, "ok");
    wait_for("the second copy", Duration::from_secs(30), || {
        dir_b.join("Welcome video (2).mp4").exists()
    });
    // A new cut, sent with replace: B's file of the name replaced (once all of it is
    // there and checked), no copy beside it (2026-11-26).
    let cut: Vec<u8> = (0..2_500_000u32).map(|i| (i * 11 % 247) as u8).collect();
    std::fs::write(dir_a.join("Welcome video.mp4"), &cut).unwrap();
    let r = a
        .request(
            Element::new("media_send")
                .with_attr("to", "b")
                .with_attr("file", "Welcome video.mp4")
                .with_attr("replace", "true"),
        )
        .unwrap();
    assert_eq!(r.name, "ok", "{}", r.to_xml());
    let offer = r.attr("offer").expect("the offer's id").to_owned();
    assert!(offer.starts_with("media-replace:"), "{offer}");
    let end = Instant::now() + Duration::from_secs(30);
    loop {
        let m = b.request(Element::new("media")).unwrap();
        let t = m
            .children_named("transfer")
            .find(|t| t.attr("way") == Some("in") && t.attr("offer") == Some(offer.as_str()))
            .expect("B's incoming transfer");
        assert_eq!(t.attr("file"), Some("Welcome video.mp4"), "{}", m.to_xml());
        if t.attr("state") == Some("received") {
            break;
        }
        assert!(
            !t.attr("state").unwrap_or("").starts_with("failed"),
            "{}",
            m.to_xml()
        );
        assert!(Instant::now() < end, "not received: {}", m.to_xml());
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(std::fs::read(dir_b.join("Welcome video.mp4")).unwrap(), cut);
    assert_eq!(
        std::fs::read(dir_b.join("Welcome video (2).mp4")).unwrap(),
        body,
        "the earlier copy left as it was"
    );
    assert!(!dir_b.join("Welcome video (3).mp4").exists());
    assert!(!dir_b.join("Welcome video.mp4.part").exists());
    wait_sent(&a, &offer, cut.len());
}

/// Waits for A's outgoing transfer `offer` to be "sent", all `size` bytes done.
fn wait_sent(a: &Node, offer: &str, size: usize) {
    let end = Instant::now() + Duration::from_secs(30);
    loop {
        let m = a.request(Element::new("media")).unwrap();
        let t = m
            .children_named("transfer")
            .find(|t| t.attr("way") == Some("out") && t.attr("offer") == Some(offer))
            .expect("A's outgoing transfer");
        let state = t.attr("state").unwrap_or("");
        if state == "sent" {
            assert_eq!(t.attr("done"), Some(size.to_string().as_str()));
            assert_eq!(t.attr("size"), Some(size.to_string().as_str()));
            return;
        }
        assert!(
            state == "offered",
            "neither offered nor sent: {}",
            m.to_xml()
        );
        assert!(Instant::now() < end, "never sent: {}", m.to_xml());
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The site's media library (§N.26, R.23): A's media folder as the library; a program on
/// B finds it, reads the catalog, fetches and prefetches items, plays one through a link
/// (byte ranges), and asks for a download.
#[test]
fn a_program_uses_the_sites_library() {
    use idn_open_protocol::library::Library;
    let auth_dir = TempDir::new("lauth");
    let authority = Authority::open(AuthorityConfig {
        state_dir: auth_dir.to_path_buf(),
        passphrase: None,
    })
    .unwrap();
    let ds = DiscoveryServer::start(0, Some(authority.clone())).unwrap();
    let server = format!("127.0.0.1:{}", ds.port());
    authority.open_pairing(Duration::from_secs(120));
    let (a, _ka) = node("la", &server, &authority);
    // (B's program changes the library's folders and settings too: B is an
    // administrator's machine.)
    let (b, _kb) = node_as("lb", &server, &authority, true);
    a.request(Element::new("set").with_attr("data.library", "true"))
        .unwrap();
    let dir = PathBuf::from(
        a.request(Element::new("media"))
            .unwrap()
            .attr("dir")
            .unwrap(),
    );
    std::fs::create_dir_all(dir.join("Sunday")).unwrap();
    let video: Vec<u8> = (0..2_000_000u32).map(|i| (i * 13 % 241) as u8).collect();
    std::fs::write(dir.join("Sunday/Welcome.mp4"), &video).unwrap();
    std::fs::write(dir.join("Background.png"), b"a picture").unwrap();
    // A program on B finds it.
    let lib = Library::find(b.enrollment().clone(), Duration::from_secs(30)).unwrap();
    assert_eq!(lib.host_name(), "la");
    // A site with more than one library: listed, and one reached by its name or its
    // machine.
    let all = Library::list(b.enrollment().clone(), Duration::from_secs(10)).unwrap();
    let la = all.iter().find(|i| i.machine == "la").expect("listed");
    assert!(la.v2 && la.fingerprint == lib.host_fingerprint(), "{all:?}");
    let named = Library::find_named(b.enrollment().clone(), &la.name, Duration::from_secs(10));
    assert_eq!(named.unwrap().host_fingerprint(), la.fingerprint);
    let on = Library::find_on(
        b.enrollment().clone(),
        &la.fingerprint,
        Duration::from_secs(10),
    );
    assert_eq!(on.unwrap().host_name(), "la");
    assert!(
        Library::find_named(
            b.enrollment().clone(),
            "No such library",
            Duration::from_secs(2)
        )
        .is_err()
    );
    let cat = lib.catalog().unwrap();
    assert_eq!(cat.folders, ["Sunday"]);
    // Items have lasting ids (v2), with their paths beside them.
    let by_path = |c: &idn_open_protocol::library::Catalog, p: &str| {
        c.items.iter().find(|i| i.path == p).cloned()
    };
    let w = by_path(&cat, "Sunday/Welcome.mp4").unwrap();
    assert_eq!(w.id.len(), 16, "{}", w.id);
    assert_eq!((w.kind.as_str(), w.size), ("video", 2_000_000));
    let bg = by_path(&cat, "Background.png").unwrap();
    // Fetched to a folder of its own, checked; prefetched again: left as it is.
    let cache = TempDir::new("cache");
    let to = cache.join("Welcome.mp4");
    let mut seen = 0;
    lib.fetch(&w.id, &to, &mut |n, _| seen = n).unwrap();
    assert_eq!(std::fs::read(&to).unwrap(), video);
    assert_eq!(seen, 2_000_000);
    let r = lib.prefetch(
        &[w.id.clone(), bg.id.clone(), "Nope.mp4".into()],
        &cache,
        &mut |_, _, _| {},
    );
    assert!(r[0].1.is_ok() && r[1].1.is_ok() && r[2].1.is_err(), "{r:?}");
    // Played directly: a link a player opens with byte ranges.
    let url = lib.link(&w.id).unwrap();
    assert!(
        url.contains("/lib/") && url.ends_with("/Welcome.mp4"),
        "{url}"
    );
    let url = url.replacen(
        url.split("://").nth(1).unwrap().split(':').next().unwrap(),
        "127.0.0.1",
        1,
    );
    let (status, body) = idn_open_protocol::web::httpc::request(
        "GET",
        &url,
        &[("Range".into(), "bytes=1000-1999".into())],
        None,
        false,
        Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(status, 206);
    assert_eq!(body, &video[1000..2000]);
    let bad = url.replace("/lib/", "/lib/0");
    let (status, _) = idn_open_protocol::web::httpc::request(
        "GET",
        &bad,
        &[],
        None,
        false,
        Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(status, 404);
    // Not a library item: refused.
    assert!(lib.link("../secret.mp4").is_err());
    // A download asked for: queued for IDN Data on A.
    let job = lib
        .download("https://example.com/v", "1080p", "Test", "Sunday")
        .unwrap();
    assert_eq!(lib.job(job).unwrap().state, "queued");
    // Links opened with a host program's long-lived site key are gone (the
    // key crossed plain HTTP): only a link asked for opens an item.
    let one = lib.link(&w.id).unwrap();
    let base = one.split("/lib/").next().unwrap().to_owned();
    let host = base
        .split("://")
        .nth(1)
        .unwrap()
        .split(':')
        .next()
        .unwrap()
        .to_owned();
    let base = base.replacen(&host, "127.0.0.1", 1);
    let (app_key, _) = authority
        .issue_token(
            "hostapp:presenter",
            "Presenter",
            "app:hostapp",
            "*",
            idn_open_protocol::trust::token::Scope::Read,
            600,
        )
        .unwrap();
    let (status, _) = idn_open_protocol::web::httpc::request(
        "GET",
        &format!("{base}/lib/item/{}/Welcome.mp4?key={app_key}", w.id),
        &[("Range".into(), "bytes=10-19".into())],
        None,
        false,
        Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(status, 404, "no keyed links");
    library_v2(&lib, &dir, &w, &video, b.enrollment().clone());
    // The library switched off: gone.
    a.request(Element::new("set").with_attr("data.library", "false"))
        .unwrap();
    assert!(lib.catalog().is_err());
}

/// Media library v2: lasting ids through a rename, the
/// SHA-256 worked out, changes to catch up on, an upload (and the same file again: the
/// item there), "in use" marks refusing a delete unless its name is typed back, the trash
/// and restore, its days, the space; and a show box's cache: pinned items fetched, states,
/// room made.
fn library_v2(
    lib: &idn_open_protocol::library::Library,
    dir: &std::path::Path,
    w: &idn_open_protocol::library::Item,
    video: &[u8],
    member: std::sync::Arc<idn_open_protocol::trust::Enrollment>,
) {
    use idn_open_protocol::library::cache::{Cache, ItemState};
    let start = lib.changes(0, Duration::ZERO).unwrap().1;
    // Renamed and moved on the library's machine: the same id.
    std::fs::create_dir_all(dir.join("Easter")).unwrap();
    std::fs::rename(dir.join("Sunday/Welcome.mp4"), dir.join("Easter/Hello.mp4")).unwrap();
    let cat = lib.catalog().unwrap();
    let moved = cat.item(&w.id).expect("the same id after a move");
    assert_eq!(moved.path, "Easter/Hello.mp4");
    let (ch, next, missed) = lib.changes(start, Duration::from_secs(2)).unwrap();
    assert!(!missed && next > start);
    assert!(
        ch.iter().any(|c| c.id == w.id && c.kind == "changed"),
        "{ch:?}"
    );
    // Its SHA-256, worked out in the background.
    wait_for("the SHA-256", Duration::from_secs(20), || {
        lib.item(&w.id).unwrap().sha256.is_some()
    });
    let sha = lib.item(&w.id).unwrap().sha256.unwrap();
    assert_eq!(sha.len(), 64);
    // An upload: a new item; the same bytes again: the item already there.
    let up_dir = TempDir::new("upload");
    let up = up_dir.join("Intro.mov");
    let bytes: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&up, &bytes).unwrap();
    let mut got = 0;
    let (uid, existing) = lib.upload(&up, "Uploads", &mut |n, _| got = n).unwrap();
    assert!(!existing && uid.len() == 16, "{uid}");
    assert_eq!(got, 300_000);
    assert_eq!(std::fs::read(dir.join("Uploads/Intro.mov")).unwrap(), bytes);
    let again_dir = TempDir::new("upload2");
    let again = again_dir.join("Copy of intro.mov");
    std::fs::write(&again, &bytes).unwrap();
    assert_eq!(
        lib.upload(&again, "", &mut |_, _| {}).unwrap(),
        (uid.clone(), true)
    );
    // In use: a delete refused (naming the holder) unless its name is typed back.
    assert_eq!(
        lib.marks("mediaapp", "set", std::slice::from_ref(&uid))
            .unwrap(),
        1
    );
    // Read without changing them.
    assert_eq!(lib.marked("mediaapp").unwrap(), vec![uid.clone()]);
    assert!(lib.marked("nobody").unwrap().is_empty());
    let e = lib.delete(&uid, "Presenter").unwrap_err().to_string();
    assert!(e.contains("mediaapp"), "{e}");
    assert!(
        lib.delete_in_use(&uid, "Presenter", "wrong", "test")
            .is_err()
    );
    let held = lib
        .delete_in_use(&uid, "Presenter", "Intro.mov", "replaced by a new cut")
        .unwrap();
    assert_eq!(held, ["mediaapp"]);
    assert!(lib.catalog().unwrap().item(&uid).is_none());
    let trash = lib.trash().unwrap();
    assert_eq!(trash["items"][0]["id"], uid.as_str());
    assert_eq!(trash["items"][0]["holders"][0], "mediaapp");
    // Restored, with its id; the marks re-synced to none; deleted plainly; restored.
    lib.restore(&uid).unwrap();
    assert!(lib.catalog().unwrap().item(&uid).is_some());
    lib.marks("mediaapp", "set", &[]).unwrap();
    lib.delete(&uid, "Presenter").unwrap();
    lib.restore(&uid).unwrap();
    // The trash's days and the space.
    lib.set_trash_days(14).unwrap();
    assert!(lib.set_trash_days(0).is_err());
    let space = lib.space().unwrap();
    assert_eq!(space["trash_days"], 14);
    assert!(space["free_bytes"].as_u64().unwrap() > 0);
    assert!(space["library_bytes"].as_u64().unwrap() >= 2_300_000);
    // A NAS share indexed in place: its file listed as `@NAS/…` with an id, fetched like
    // any item, never deleted by the library.
    let nas = TempDir::new("nas");
    std::fs::write(nas.join("Carols.mp4"), b"carols on the share").unwrap();
    lib.add_root("NAS", nas.to_str().unwrap()).unwrap();
    let cat = lib.catalog().unwrap();
    let n = cat
        .items
        .iter()
        .find(|i| i.path == "@NAS/Carols.mp4")
        .expect("indexed");
    let got_dir = TempDir::new("nas-got");
    let got = got_dir.join("c.mp4");
    lib.fetch(&n.id, &got, &mut |_, _| {}).unwrap();
    assert_eq!(std::fs::read(&got).unwrap(), b"carols on the share");
    assert!(lib.delete(&n.id, "Presenter").is_err());
    assert!(nas.join("Carols.mp4").exists());
    // A link to it plays it (the item found through the index, not the media folder).
    assert!(lib.link(&n.id).unwrap().ends_with("/Carols.mp4"));
    // Its file's name is its path's, not its id.
    assert_eq!(n.name(), "Carols.mp4");
    lib.remove_root("NAS").unwrap();
    assert!(lib.catalog().unwrap().item(&n.id).is_none());
    // A show box's cache: two items pinned (fetched, checked), a cap that holds one: the
    // pinned stay; unpinned, the oldest goes.
    let lib = std::sync::Arc::new(
        idn_open_protocol::library::Library::find(member.clone(), Duration::from_secs(30)).unwrap(),
    );
    let cdir = TempDir::new("showcache");
    let c = Cache::open(&cdir, 10_000_000, lib.clone()).unwrap();
    c.pin(&[w.id.clone(), uid.clone()]);
    let end = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let s = c.states();
        if matches!(s.get(&w.id), Some(ItemState::Cached(_)))
            && matches!(s.get(&uid), Some(ItemState::Cached(_)))
        {
            break;
        }
        assert!(std::time::Instant::now() < end, "not cached: {s:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(std::fs::read(c.path(&w.id).unwrap()).unwrap(), video);
    // The item rewritten in the library: the cache hears of it through the changes (not a
    // whole catalog) and fetches it again.
    let video2: Vec<u8> = (0..2_000_100u32).map(|i| (i * 7 % 239) as u8).collect();
    let now_at = lib.item(&w.id).unwrap().path;
    std::fs::write(dir.join(&now_at), &video2).unwrap();
    lib.catalog().unwrap();
    let end = std::time::Instant::now() + Duration::from_secs(30);
    while c
        .path(&w.id)
        .is_none_or(|p| std::fs::read(p).ok().as_ref() != Some(&video2))
    {
        assert!(
            std::time::Instant::now() < end,
            "never fetched again: {:?} {:?}",
            c.state(&w.id),
            lib.item(&w.id)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    c.set_cap(400_000);
    c.pin(std::slice::from_ref(&uid));
    wait_for("room made", Duration::from_secs(10), || {
        c.path(&w.id).is_none()
    });
    assert!(c.path(&uid).is_some(), "the pinned one stays");
    assert_eq!(c.state("ffffffffffffffff"), ItemState::NotKept);
    // Over the program's own channel (one endpoint toward the library's machine;
    // 2026-11-22): an item asked for right after it was added waits (not
    // "missing") and is looked for at once, not at the cache's next look 5 s on; one the
    // library doesn't have: missing once looked for.
    struct Quiet;
    impl idn_open_protocol::channel::Handler for Quiet {}
    let own = idn_open_protocol::channel::Channel::start(
        idn_open_protocol::channel::ChannelConfig {
            client_only: true,
            ..idn_open_protocol::channel::ChannelConfig::new(member)
        },
        std::sync::Arc::new(Quiet),
    )
    .unwrap();
    let lib2 = std::sync::Arc::new(
        idn_open_protocol::library::Library::find_on_via(
            own,
            lib.host_fingerprint(),
            Duration::from_secs(30),
        )
        .unwrap(),
    );
    let cdir2 = TempDir::new("showcache2");
    let c2 = Cache::open(&cdir2, 10_000_000, lib2.clone()).unwrap();
    // (Its first look at the library: the catalog read whole.)
    c2.want("ffffffffffffffff");
    wait_for("the first look", Duration::from_secs(10), || {
        c2.state("ffffffffffffffff") == ItemState::Missing
    });
    let fresh = TempDir::new("fresh");
    std::fs::write(fresh.join("Fresh.mp4"), b"just added").unwrap();
    let (fid, _) = lib2
        .upload(&fresh.join("Fresh.mp4"), "", &mut |_, _| {})
        .unwrap();
    let asked = std::time::Instant::now();
    c2.want(&fid);
    assert_ne!(
        c2.state(&fid),
        ItemState::Missing,
        "asked for, not looked for yet"
    );
    while !matches!(c2.state(&fid), ItemState::Cached(_)) {
        assert!(
            asked.elapsed() < Duration::from_secs(4),
            "not looked for at once: {:?}",
            c2.state(&fid)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    c2.want("eeeeeeeeeeeeeeee");
    let end = std::time::Instant::now() + Duration::from_secs(4);
    while c2.state("eeeeeeeeeeeeeeee") != ItemState::Missing {
        assert!(
            std::time::Instant::now() < end,
            "{:?}",
            c2.state("eeeeeeeeeeeeeeee")
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
