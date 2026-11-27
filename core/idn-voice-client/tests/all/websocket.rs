//! Over the WebSocket against the real server: one handset talks, another hears it
//! through its speaker (level, pitch, delay); a dropped link resumes with its token and
//! `have` and plays what came meanwhile; a revoked or unknown credential ends the session
//! without trying again.

use crate::common::{Ear, Relay, Server, block_rms, client, crossings_per_s, speak};
use idn_voice_client::{Event, Target, Transport};
use serde_json::json;
use std::time::Duration;

const S5: Duration = Duration::from_secs(5);

#[test]
fn a_handset_talks_and_another_hears_it_through_the_speaker() {
    let srv = Server::start();
    let crew = srv.channel("Crew");
    let (a, ae) = client(&srv.provision("Truck 1"), json!({"urls": [srv.url()]}));
    let (b, be) = client(&srv.provision("Truck 2"), json!({"urls": [srv.url()]}));
    ae.wait("connected", 0, S5, |e| {
        matches!(e, Event::Connected(Transport::WebSocket))
    });
    let (_, resumed, hello) = ae.hello(0);
    assert!(!resumed);
    assert_eq!(hello["me"]["handset"]["name"], "Truck 1");
    assert_eq!(hello["media"], "websocket");
    be.hello(0);
    a.join(crew, None);
    b.join(crew, None);
    for ev in [&ae, &be] {
        ev.wait(
            "joined",
            0,
            S5,
            |e| matches!(e, Event::Joined { channel, .. } if *channel == crew),
        );
    }

    let ear = Ear::start(b.clone());
    a.talk_start(crew);
    ae.wait("talk on", 0, S5, |e| {
        matches!(e, Event::Talk { on: true, .. })
    });
    be.wait("a talker", 0, S5, |e| {
        matches!(e, Event::Talking { channel, talkers }
            if *channel == crew && talkers.iter().any(|t| t.name == "Handset Truck 1"))
    });
    let t0 = speak(&a, 1000);
    a.talk_stop();
    ae.wait(
        "talk off",
        0,
        S5,
        |e| matches!(e, Event::Talk { on: false, why: Some(w) } if w == "released"),
    );
    std::thread::sleep(Duration::from_millis(800));
    let s = ear.samples();
    let started = ear.started;
    drop(ear);

    let rms = block_rms(&s);
    let first = rms.iter().position(|&r| r > 0.05).expect("the tone heard");
    // The site's jitter buffer starts at 200 ms; loopback adds little. (Block k was
    // pulled about 20·k ms after the ear started.)
    let heard = (started + Duration::from_millis(20) * first as u32).saturating_duration_since(t0);
    assert!(
        heard >= Duration::from_millis(150) && heard < Duration::from_millis(1000),
        "heard {heard:?} after the first sample"
    );
    let loud = rms.iter().filter(|&&r| r > 0.1).count();
    assert!((40..=58).contains(&loud), "{loud} loud blocks for 50 sent");
    // The steady part: the tone's level (0.21 RMS) and pitch (440 Hz: 880 crossings).
    let steady = &s[(first + 10) * 960..(first + 35) * 960];
    let level = block_rms(steady).iter().sum::<f64>() / 25.0;
    assert!((0.12..0.3).contains(&level), "level {level}");
    let cps = crossings_per_s(steady);
    assert!((820.0..940.0).contains(&cps), "{cps} crossings a second");
    let st = b.stats();
    assert!(st.packets >= 48, "{st:?}");
    assert_eq!((st.lost, st.late, st.duplicates), (0, 0, 0), "{st:?}");
    assert_eq!(a.stats().packets, 0, "a talker doesn't hear itself");

    // Text and SOS on the same session.
    a.message(Target::Channel(crew), "Loaded, leaving the plant");
    be.wait(
        "message",
        0,
        S5,
        |e| matches!(e, Event::Message(m) if m["text"] == "Loaded, leaving the plant"),
    );
    a.sos(Some((53.3498, -6.2603)), Some("slid off the road"));
    ae.wait(
        "sos_sent",
        0,
        S5,
        |e| matches!(e, Event::Sos(m) if m["t"] == "sos_sent"),
    );
    a.close();
    b.close();
}

#[test]
fn a_dropped_link_resumes_with_have_and_plays_what_came_meanwhile() {
    let srv = Server::start();
    let crew = srv.channel("Crew");
    let relay = Relay::start(&srv.rt, srv.addr);
    let (a, ae) = client(&srv.provision("Truck 1"), json!({"urls": [srv.url()]}));
    let (b, be) = client(&srv.provision("Truck 2"), json!({"urls": [relay.url()]}));
    let (_, _, a_hello) = ae.hello(0);
    let (_, _, b_hello) = be.hello(0);
    a.join(crew, None);
    b.join(crew, None);
    for ev in [&ae, &be] {
        ev.wait(
            "joined",
            0,
            S5,
            |e| matches!(e, Event::Joined { channel, .. } if *channel == crew),
        );
    }
    let ear = Ear::start(b.clone());
    a.talk_start(crew);
    ae.wait("talk on", 0, S5, |e| {
        matches!(e, Event::Talk { on: true, .. })
    });
    // Heard live first: `have` then holds a's last packet.
    speak(&a, 400);
    std::thread::sleep(Duration::from_millis(500));
    let before = b.resume_state().expect("a token");
    assert_eq!(Some(before.token.as_str()), b_hello["resume"].as_str());
    let a_sid = a_hello["sid"].as_u64().unwrap();
    assert!(
        before.have.starts_with(&format!("{a_sid}:")),
        "have {:?}",
        before.have
    );

    // The link goes; a talks on meanwhile.
    let n = be.all().len();
    relay.cut();
    be.wait("reconnecting", n, S5, |e| {
        matches!(e, Event::Reconnecting { .. })
    });
    speak(&a, 1000);
    a.talk_stop();
    relay.restore();
    let (_, resumed, hello) = be.hello(n);
    assert!(resumed, "{hello}");
    assert!(hello["backlog"].as_u64().unwrap() >= 40, "{hello}");
    assert_ne!(hello["resume"], b_hello["resume"], "a new token");
    assert_eq!(
        hello["joined"][0]["channel"].as_u64(),
        Some(u64::from(crew)),
        "still in the channel"
    );
    // The backlog plays at once and catches up.
    std::thread::sleep(Duration::from_secs(2));
    let rms = block_rms(&ear.samples());
    drop(ear);
    let loud = rms.iter().filter(|&&r| r > 0.1).count();
    // 20 live, then 50 kept, played a little faster: about 60 blocks.
    assert!(loud >= 50, "{loud} loud blocks");
    let st = b.stats();
    assert!(st.packets >= 65, "{st:?}");
    assert!(st.duplicates + st.late <= 3, "have sent: {st:?}");
    assert!(
        b.resume_state()
            .unwrap()
            .have
            .starts_with(&format!("{a_sid}:"))
    );
}

#[test]
fn a_talk_through_a_dead_zone_is_held_and_heard_after_the_resume() {
    let srv = Server::start();
    let crew = srv.channel("Crew");
    let relay = Relay::start(&srv.rt, srv.addr);
    // The talker is the one whose link drops.
    let (a, ae) = client(&srv.provision("Truck 1"), json!({"urls": [relay.url()]}));
    let (b, be) = client(&srv.provision("Truck 2"), json!({"urls": [srv.url()]}));
    ae.hello(0);
    be.hello(0);
    a.join(crew, None);
    b.join(crew, None);
    for ev in [&ae, &be] {
        ev.wait(
            "joined",
            0,
            S5,
            |e| matches!(e, Event::Joined { channel, .. } if *channel == crew),
        );
    }
    let ear = Ear::start(b.clone());
    a.talk_start(crew);
    ae.wait("talk on", 0, S5, |e| {
        matches!(e, Event::Talk { on: true, .. })
    });
    speak(&a, 400);

    // Into a dead zone: the driver talks on for a second and lets go before it's back.
    let (n, nb) = (ae.all().len(), be.all().len());
    relay.cut();
    ae.wait("reconnecting", n, S5, |e| {
        matches!(e, Event::Reconnecting { .. })
    });
    speak(&a, 1000);
    let held = a.stats();
    assert!(held.mic_held >= 45, "{held:?}");
    a.talk_stop();
    relay.restore();
    let (_, resumed, _) = ae.hello(n);
    assert!(resumed);
    // The talk is started again and the held second sent (4× real time), then its end.
    be.wait("a's talk again", nb, Duration::from_secs(10), |e| {
        matches!(e, Event::Talking { channel, talkers }
            if *channel == crew && talkers.iter().any(|t| t.name == "Handset Truck 1"))
    });
    std::thread::sleep(Duration::from_secs(3));
    let rms = block_rms(&ear.samples());
    drop(ear);
    let loud = rms.iter().filter(|&&r| r > 0.1).count();
    // 20 live, then the 50 held, heard late (a burst, played at normal speed and caught
    // up on): about 60 blocks.
    assert!(loud >= 55, "{loud} loud blocks");
    let st = a.stats();
    assert_eq!((st.mic_held, st.mic_dropped), (0, 0), "{st:?}");
    assert!(b.stats().packets >= 65, "{:?}", b.stats());
}

/// A link that goes silent without a word (no FIN, no RST: a cellular dead zone) is
/// noticed at hello's idle timeout, and the session resumed.
#[test]
fn a_link_gone_silent_is_noticed_at_the_idle_timeout_and_resumed() {
    let srv = Server::start();
    let (st, _, v) = srv.http(
        "POST",
        "/voice/api/settings",
        &json!({"idle_timeout_s": 20, "ping_s": 5}),
    );
    assert_eq!(st, 200, "{v}");
    let relay = Relay::start(&srv.rt, srv.addr);
    let (c, ev) = client(&srv.provision("Truck 8"), json!({"urls": [relay.url()]}));
    let (_, _, hello) = ev.hello(0);
    assert_eq!(hello["link"]["idle_timeout_s"], 20, "{hello}");
    let n = ev.all().len();
    let t0 = std::time::Instant::now();
    relay.silence();
    let (i, e) = ev.wait("the silence noticed", n, Duration::from_secs(30), |e| {
        matches!(e, Event::Reconnecting { .. })
    });
    // The last message (a pong, every 5 s) up to 5 s before: 15 to 20 s.
    let took = t0.elapsed();
    assert!(
        took >= Duration::from_secs(14) && took < Duration::from_secs(26),
        "{took:?}"
    );
    assert!(
        matches!(&e, Event::Reconnecting { why, .. } if why.contains("nothing from the server")),
        "{e:?}"
    );
    let (_, resumed, _) = ev.hello(i);
    assert!(resumed);
    assert_eq!(c.transport(), Some(Transport::WebSocket));
}

#[test]
fn a_revoked_or_unknown_credential_ends_the_session_for_good() {
    let srv = Server::start();
    let answer = srv.provision("Truck 7");
    let id = answer["handset"]["id"].as_str().unwrap().to_owned();
    let (c, ev) = client(&answer, json!({"urls": [srv.url()]}));
    ev.hello(0);
    let (st, _, _) = srv.http(
        "POST",
        &format!("/voice/api/handsets/{id}/revoke"),
        &json!({}),
    );
    assert_eq!(st, 200);
    ev.wait(
        "closed",
        0,
        S5,
        |e| matches!(e, Event::Closed { why } if why.contains("revoked")),
    );
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        !ev.all()
            .iter()
            .any(|e| matches!(e, Event::Reconnecting { .. })),
        "{:#?}",
        ev.all()
    );
    assert_eq!(c.transport(), None);

    // An unknown credential: refused at once, not tried again.
    // (Kept: a client dropped closes its session.)
    let (_c2, ev) = client(
        &json!({"credential": "idnv1.nope.nope"}),
        json!({"urls": [srv.url()]}),
    );
    ev.wait("refused", 0, S5, |e| matches!(e, Event::Closed { .. }));
    assert!(
        !ev.all()
            .iter()
            .any(|e| matches!(e, Event::Reconnecting { .. })),
        "{:#?}",
        ev.all()
    );
}
