//! Against Voice's own QUIC listener (`idn-voice/1` on the link, `Voice::serve_link`):
//! built beside this crate by another session (2026-11-26), so behind the feature
//! `quic-server-tests` until both have compiled together:
//! `cargo nextest run -p idn-voice-client --features quic-server-tests`.

use crate::common::{Ear, Server, block_rms, client, speak};
use crate::quic::fake;
use idn_common::sync::LockOk as _;
use idn_open_protocol::link::LinkConfig;
use idn_voice_client::{Event, Transport};
use serde_json::json;
use std::sync::atomic::Ordering;
use std::time::Duration;

const S5: Duration = Duration::from_secs(5);

/// Voice's link listener on loopback with a key of the test's own (hold it: dropped, it
/// stops).
fn link_listener(srv: &Server) -> idn_voice::VoiceLink {
    let _in = srv.rt.enter();
    srv.voice
        .serve_link(
            "127.0.0.1:0".parse().unwrap(),
            &LinkConfig::ephemeral(),
            srv.rt.handle(),
        )
        .unwrap()
}

/// The provisioning answer's link and pin, as the app passes them on.
fn quic_of(answer: &serde_json::Value) -> serde_json::Value {
    assert!(answer["link"][0].is_string(), "{answer}");
    json!({"link": answer["link"], "link_cert_sha256": answer["link_cert_sha256"]})
}

#[test]
fn a_handset_on_quic_and_one_on_the_websocket_hear_each_other() {
    let srv = Server::start();
    let crew = srv.channel("Crew");
    let _link = link_listener(&srv);
    let qa = srv.provision("Truck 1");
    let mut extra = quic_of(&qa);
    extra["transport"] = json!("quic");
    let (q, qe) = client(&qa, extra);
    let (w, we) = client(&srv.provision("Truck 2"), json!({"urls": [srv.url()]}));
    qe.wait("on QUIC", 0, S5, |e| {
        matches!(e, Event::Connected(Transport::Quic))
    });
    let (_, _, hello) = qe.hello(0);
    assert_eq!(hello["me"]["handset"]["name"], "Truck 1");
    assert!(hello["link"]["link_connect_ms"].is_u64(), "{hello}");
    we.hello(0);
    q.join(crew, None);
    w.join(crew, None);
    for ev in [&qe, &we] {
        ev.wait(
            "joined",
            0,
            S5,
            |e| matches!(e, Event::Joined { channel, .. } if *channel == crew),
        );
    }
    // QUIC talks, the WebSocket hears; then the other way round.
    for (talker, tev, listener) in [(&q, &qe, &w), (&w, &we, &q)] {
        let ear = Ear::start(listener.clone());
        let n = tev.all().len();
        talker.talk_start(crew);
        tev.wait("talk on", n, S5, |e| {
            matches!(e, Event::Talk { on: true, .. })
        });
        speak(talker, 600);
        talker.talk_stop();
        std::thread::sleep(Duration::from_millis(600));
        let loud = block_rms(&ear.samples())
            .iter()
            .filter(|&&r| r > 0.1)
            .count();
        assert!(loud >= 25, "{loud} loud blocks for 30 sent");
    }
}

#[test]
fn a_network_change_keeps_the_quic_session() {
    let srv = Server::start();
    let crew = srv.channel("Crew");
    let _link = link_listener(&srv);
    let qa = srv.provision("Truck 1");
    let mut extra = quic_of(&qa);
    extra["transport"] = json!("quic");
    let (q, qe) = client(&qa, extra);
    let (w, we) = client(&srv.provision("Truck 2"), json!({"urls": [srv.url()]}));
    qe.hello(0);
    we.hello(0);
    q.join(crew, None);
    w.join(crew, None);
    for ev in [&qe, &we] {
        ev.wait(
            "joined",
            0,
            S5,
            |e| matches!(e, Event::Joined { channel, .. } if *channel == crew),
        );
    }
    // A new socket (a new source port): the connection migrates, no resume.
    let n = qe.all().len();
    q.network_changed();
    std::thread::sleep(Duration::from_secs(1));
    let ear = Ear::start(q.clone());
    w.talk_start(crew);
    we.wait("talk on", 0, S5, |e| {
        matches!(e, Event::Talk { on: true, .. })
    });
    speak(&w, 600);
    w.talk_stop();
    std::thread::sleep(Duration::from_millis(600));
    let loud = block_rms(&ear.samples())
        .iter()
        .filter(|&&r| r > 0.1)
        .count();
    assert!(loud >= 25, "{loud} loud blocks for 30 sent");
    let after = &qe.all()[n..];
    assert!(
        !after
            .iter()
            .any(|e| matches!(e, Event::Reconnecting { .. } | Event::Hello { .. })),
        "{after:#?}"
    );
    assert_eq!(q.transport(), Some(Transport::Quic));
}

/// What a hello says of the link is believed only over a pinned connection: over plain
/// HTTP (these tests' server) a client stays on the WebSocket; given the enrolment's
/// link and pin it goes over QUIC first.
#[test]
fn the_link_is_learnt_only_from_a_pinned_server_and_used_when_configured() {
    let srv = Server::start();
    let (st, _, v) = srv.http("POST", "/voice/api/settings", &json!({"link_retry_s": 5}));
    assert_eq!(st, 200, "{v}");
    let _link = link_listener(&srv);
    let (c, ev) = client(&srv.provision("Truck 3"), json!({"urls": [srv.url()]}));
    ev.wait("on the WebSocket", 0, S5, |e| {
        matches!(e, Event::Connected(Transport::WebSocket))
    });
    let (_, _, first) = ev.hello(0);
    assert!(first["link"]["quic"]["cert_sha256"].is_string(), "{first}");
    // Two retries' time: nothing learnt, nothing tried.
    std::thread::sleep(Duration::from_secs(7));
    assert_eq!(c.transport(), Some(Transport::WebSocket));
    // The enrolment's link and pin: QUIC at once.
    let answer = srv.provision("Truck 5");
    let mut extra = quic_of(&answer);
    extra["urls"] = json!([srv.url()]);
    let (q, qe) = client(&answer, extra);
    qe.wait("on QUIC", 0, S5, |e| {
        matches!(e, Event::Connected(Transport::Quic))
    });
    assert_eq!(q.transport(), Some(Transport::Quic));
}

/// The client's side of a move with the server's WebSocket and the tests' own QUIC
/// listener, which Voice offers (`offer_link`): QUIC refused at first (the WebSocket),
/// then working (the session moves, resumed with the WebSocket's token).
#[test]
fn the_websocket_when_quic_fails_then_the_session_moves_to_quic() {
    let srv = Server::start();
    let (st, _, v) = srv.http("POST", "/voice/api/settings", &json!({"link_retry_s": 5}));
    assert_eq!(st, 200, "{v}");
    let answer = srv.provision("Truck 4");
    let f = fake(&srv.rt);
    srv.voice.offer_link(Some((f.addr.port(), f.pin.as_str())));
    f.open.store(false, Ordering::SeqCst);
    let (c, ev) = client(
        &answer,
        json!({"urls": [srv.url()], "link": [f.addr.to_string()], "link_cert_sha256": f.pin,
               "link_connect_ms": 1000}),
    );
    ev.wait("on the websocket", 0, S5, |e| {
        matches!(e, Event::Connected(Transport::WebSocket))
    });
    let (_, _, hello) = ev.hello(0);
    assert!(!f.auths.lock_ok().is_empty(), "QUIC was tried first");
    assert_eq!(hello["link"]["link_retry_s"], 5, "{hello}");
    f.open.store(true, Ordering::SeqCst);
    let (i, _) = ev.wait("moved to QUIC", 0, Duration::from_secs(10), |e| {
        matches!(e, Event::Connected(Transport::Quic))
    });
    let (_, _, moved) = ev.hello(i);
    assert_eq!(moved["resume"], "fake-token");
    // The move resumed the WebSocket's session: its token and credential.
    let last = f.auths.lock_ok().last().cloned().unwrap();
    assert_eq!(last["resume"], hello["resume"]);
    assert_eq!(last["credential"], answer["credential"]);
    assert_eq!(c.transport(), Some(Transport::Quic));
    c.close();
}
