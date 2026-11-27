//! Discovery Server end-to-end (spec chapter 8): our server with our source-protocol and
//! receiver-protocol clients.

use crate::common;
use common::idn;

use idn_open_protocol::discovery::ds::{self, Role, Session, SourceChange, SourceWatcher, Subject};
use idn_open_protocol::discovery::ds_server;
use idn_open_protocol::discovery::responder::Published;
use idn_open_protocol::xml::Element;
use std::net::TcpListener;
use std::time::Duration;

#[test]
fn sources_receivers_control_and_monitor() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || ds_server::serve(l));
    let t = Duration::from_secs(3);

    // Source protocol: a watching finder sees the registration come and go.
    let mut w = SourceWatcher::connect(addr).unwrap();
    w.set_timeout(Some(t)).unwrap();
    assert_eq!(w.next_change().unwrap(), Some(SourceChange::All(vec![])));
    let reg = ds::register_source(
        addr,
        &Published {
            name: "SND (LAB)".into(),
            port: 5961,
            groups: vec!["public".into()],
            routing: false,
            idn_ext: Some("resume/1 secure/1".into()),
        },
    )
    .unwrap();
    let Some(SourceChange::Added(s)) = w.next_change().unwrap() else {
        panic!()
    };
    assert_eq!(
        (s.name.as_str(), s.socket_addr()),
        ("SND (LAB)", Some("127.0.0.1:5961".parse().unwrap()))
    );
    // Discovery hint as <metadata> text (§E.2.2).
    assert_eq!(s.idn_ext, ["resume/1", "secure/1"]);
    let listed = ds::query_sources(addr, t).unwrap();
    assert_eq!(listed, vec![s.clone()]);
    drop(reg);
    assert_eq!(w.next_change().unwrap(), Some(SourceChange::Removed(s)));

    // Receiver protocol: advertise, list, control, monitor.
    let mut adv = Session::connect(addr, Subject::Receiver, Role::Advertiser, t).unwrap();
    let rid = ds::uuid4();
    adv.send(&ds::receiver_element(
        &rid,
        "RCV (CLEAN)",
        "video",
        &ds::uuid4(),
        "Studio",
    ))
    .unwrap();
    let mut lis = Session::connect(addr, Subject::Receiver, Role::Listener, t).unwrap();
    let lid = lis.listener_id.clone().expect("listener id");
    std::thread::sleep(Duration::from_millis(100));
    lis.send(&Element::new("query_receivers")).unwrap();
    lis.set_timeout(Some(t)).unwrap();
    let list = lis.recv().unwrap().unwrap();
    let recv = &list[0].children[0];
    assert_eq!(recv.child_text("receiver_id"), rid);
    assert_eq!(recv.child_text("address"), "127.0.0.1");
    assert_eq!(lis.recv().unwrap().unwrap()[0].name, "monitor_events");

    lis.send(&ds::connect_command(&rid, "SND (LAB)")).unwrap();
    adv.set_timeout(Some(t)).unwrap();
    let cmd = adv.recv().unwrap().unwrap();
    assert_eq!(ds::parse_connect_command(&cmd[0], &rid), Some("SND (LAB)"));

    let req = Element::new("monitor_request")
        .with_attr("unsubscribe", "false")
        .with_child(Element::new("receiver_id").with_text(&rid))
        .with_child(Element::new("listener_id"))
        .with_child(
            Element::new("monitor_capabilities")
                .with_child(Element::new("property").with_attr("name", "connection-state")),
        );
    lis.send(&req).unwrap();
    let fwd = adv.recv().unwrap().unwrap();
    assert_eq!(fwd[0].name, "monitor_request");
    assert_eq!(fwd[0].child_text("listener_id"), lid);
    adv.send(&ds::monitor_events(
        Subject::Receiver,
        &lid,
        &rid,
        &[("connection-state", "connected".into())],
    ))
    .unwrap();
    let ev = lis.recv().unwrap().unwrap();
    let prop = &ev[0]
        .child("listener")
        .unwrap()
        .child("receiver")
        .unwrap()
        .children[0];
    assert_eq!(
        (prop.attr("name"), prop.attr("value")),
        (Some("connection-state"), Some("connected"))
    );

    drop(lis);
    let bye = adv.recv().unwrap().unwrap();
    assert_eq!(bye[0].name, "remove_listener");
    assert_eq!(bye[0].text, lid);
}

/// The pieces together: the Discovery Server, idn-recv --advertise (no source given),
/// a scripted sender registered on the server, and a listener that tells the receiver
/// which source to use and monitors it.
#[test]
fn advertised_receiver_is_told_where_to_connect() {
    use idn_open_protocol::frame::{Frame, Limits, kind, read_frame};
    use idn_open_protocol::media::VideoHeader;
    use std::io::Write;
    use std::process::Stdio;

    // The server in-process: idn-discovery-server (the Site Manager's crate) serves the
    // same `ds_server::serve`.
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || ds_server::serve(l));
    let dsaddr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

    // Scripted sender, registered as "SND (DS)".
    let snd = TcpListener::bind("127.0.0.1:0").unwrap();
    let snd_port = snd.local_addr().unwrap().port();
    let _reg = ds::register_source(
        dsaddr,
        &Published {
            name: "SND (DS)".into(),
            port: snd_port,
            groups: vec!["public".into()],
            routing: false,
            idn_ext: None,
        },
    )
    .unwrap();
    let sender = std::thread::spawn(move || {
        let (mut s, _) = snd.accept().unwrap();
        let mut rx = s.try_clone().unwrap();
        for _ in 0..5 {
            read_frame(&mut rx, &Limits::default()).unwrap().unwrap();
        }
        let payload = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../core/idn-open-protocol/tests/vectors/shq/v2-new-shq2-ramp-64x16.payload.bin"
        ))
        .unwrap();
        let vh = VideoHeader {
            fourcc: *b"SHQ2",
            width: 64,
            height: 16,
            frame_rate_n: 25,
            frame_rate_d: 1,
            aspect_ratio: 4.0,
            field_id: 0,
            stream: 1,
            timecode: 0,
            timestamp: 0,
            metadata: None,
        };
        for _ in 0..3 {
            s.write_all(
                &Frame {
                    kind: kind::VIDEO,
                    scrambled: true,
                    version: 0,
                    ext: vh.encode(),
                    payload: payload.clone(),
                }
                .encode(),
            )
            .unwrap();
        }
        let _ = read_frame(&mut rx, &Limits::default());
    });

    let out = idn_common::testing::TempDir::new("idn-ds-recv");
    let recv = idn("recv")
        .args([
            "--server",
            &format!("127.0.0.1:{port}"),
            "--advertise",
            "--name",
            "DSTEST",
            "--transport",
            "inline",
            "--seconds",
            "4",
            "--out",
            out.to_str().unwrap(),
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    // Listener: find the receiver, tell it to connect, then monitor it.
    let t = Duration::from_secs(3);
    let mut rid = String::new();
    for _ in 0..30 {
        let mut l = Session::connect(dsaddr, Subject::Receiver, Role::Listener, t).unwrap();
        l.send(&Element::new("query_receivers")).unwrap();
        l.set_timeout(Some(t)).unwrap();
        let list = l.recv().unwrap().unwrap();
        if let Some(r) = list[0]
            .children_named("receiver")
            .find(|r| r.child_text("name").ends_with("(DSTEST)"))
        {
            rid = r.child_text("receiver_id").to_owned();
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!rid.is_empty(), "receiver advertised");
    let mut l = Session::connect(dsaddr, Subject::Receiver, Role::Listener, t).unwrap();
    l.send(&ds::connect_command(&rid, "SND (DS)")).unwrap();
    l.send(
        &Element::new("monitor_request")
            .with_attr("unsubscribe", "false")
            .with_child(Element::new("receiver_id").with_text(&rid))
            .with_child(Element::new("listener_id"))
            // Name the properties: with an empty list NDI 6.3 advertisers (and ours) report
            // nothing (§8.3.6.1).
            .with_child(
                Element::new("monitor_capabilities")
                    .with_child(Element::new("property").with_attr("name", "connection-state"))
                    .with_child(Element::new("property").with_attr("name", "video-resolution")),
            ),
    )
    .unwrap();
    l.set_timeout(Some(Duration::from_millis(500))).unwrap();
    let mut props = std::collections::BTreeMap::new();
    let end = std::time::Instant::now() + Duration::from_secs(4);
    // (Until the resolution is known: before the first picture it is reported empty.)
    while std::time::Instant::now() < end
        && props
            .get("video-resolution")
            .is_none_or(|v: &String| v.is_empty())
    {
        if let Ok(Some(msgs)) = l.recv() {
            for m in msgs {
                for r in m
                    .child("listener")
                    .into_iter()
                    .flat_map(|x| x.children_named("receiver"))
                {
                    for p in r.children_named("property") {
                        props.insert(
                            p.attr("name").unwrap().to_owned(),
                            p.attr("value").unwrap().to_owned(),
                        );
                    }
                }
            }
        }
    }
    let o = recv.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&o.stdout);
    println!("{stdout}\n{props:?}");
    sender.join().unwrap();
    assert!(o.status.success(), "{stdout}");
    assert!(stdout.contains("connect command: \"SND (DS)\""), "{stdout}");
    assert!(stdout.contains("via discovery server"), "{stdout}");
    assert!(stdout.contains("video messages decoded: 3"), "{stdout}");
    assert_eq!(
        props.get("connection-state").map(String::as_str),
        Some("connected")
    );
    assert_eq!(
        props.get("video-resolution").map(String::as_str),
        Some("64x16")
    );
}
