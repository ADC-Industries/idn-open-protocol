//! IDN Voice over WebRTC (§V.8): two clients (str0m as a browser would be), each a
//! microphone track and six talker slots, connected through the control WebSocket's
//! offer and answer to the server's one UDP port; one talks, the other gets its packets
//! unchanged on a slot, told which talker it carries.

use futures_util::{SinkExt, StreamExt};
use idn_voice::{Host, User, Voice, VoiceConfig};
use serde_json::{Value, json};
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};
use str0m::change::SdpAnswer;
use str0m::format::Codec;
use str0m::media::{Direction, Frequency, MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc};
use tokio_tungstenite::tungstenite::Message;

struct TestHost;

impl Host for TestHost {
    fn session_user(&self, _: &axum::http::HeaderMap) -> Option<User> {
        None
    }
    fn login(&self, _: &str, _: &str, _: std::net::IpAddr) -> Option<User> {
        None
    }
    fn users(&self) -> Vec<User> {
        vec![]
    }
    fn audit(&self, _: &str, _: &str, _: Value) {}
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn wait(ws: &mut Ws, t: &str, held: &mut Vec<Value>) -> Value {
    if let Some(i) = held.iter().position(|m| m["t"] == t) {
        return held.remove(i);
    }
    loop {
        let m = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .unwrap_or_else(|_| panic!("no {t}"))
            .unwrap()
            .unwrap();
        if let Message::Text(x) = m {
            let v: Value = serde_json::from_str(&x).unwrap();
            if v["t"] == t {
                return v;
            }
            held.push(v);
        }
    }
}

/// A WebRTC client: its connection, socket and media.
struct Peer {
    rtc: Rtc,
    sock: UdpSocket,
    mic: Mid,
    slots: Vec<Mid>,
}

fn peer_offer() -> (
    Rtc,
    UdpSocket,
    Mid,
    Vec<Mid>,
    String,
    str0m::change::SdpPendingOffer,
) {
    let mut rtc = Rtc::builder()
        .clear_codecs()
        .enable_opus(true, false)
        .build(Instant::now());
    let sock = UdpSocket::bind("0.0.0.0:0").unwrap();
    // This machine's address (not loopback: as a browser would offer).
    let probe = UdpSocket::bind("0.0.0.0:0").unwrap();
    probe.connect("192.0.2.1:9").unwrap();
    let ip = probe.local_addr().unwrap().ip();
    let local = SocketAddr::new(ip, sock.local_addr().unwrap().port());
    rtc.add_local_candidate(Candidate::host(local, "udp").unwrap());
    let mut api = rtc.sdp_api();
    let mic = api.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
    let slots: Vec<Mid> = (0..6)
        .map(|_| api.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None))
        .collect();
    let (offer, pending) = api.apply().unwrap();
    (rtc, sock, mic, slots, offer.to_sdp_string(), pending)
}

/// Drives a peer for a moment: its output sent, its input taken; media data handed back.
fn step(p: &mut Peer) -> Vec<(Mid, Vec<u8>)> {
    let mut got = Vec::new();
    let mut buf = vec![0u8; 2000];
    loop {
        match p.rtc.poll_output().unwrap() {
            Output::Timeout(_) => break,
            Output::Transmit(t) => {
                let _ = p.sock.send_to(&t.contents, t.destination);
            }
            Output::Event(Event::MediaData(d)) => got.push((d.mid, d.data.to_vec())),
            Output::Event(_) => {}
        }
    }
    p.sock
        .set_read_timeout(Some(Duration::from_millis(2)))
        .unwrap();
    if let Ok((n, source)) = p.sock.recv_from(&mut buf) {
        let destination = SocketAddr::new(
            {
                let probe = UdpSocket::bind("0.0.0.0:0").unwrap();
                probe.connect("192.0.2.1:9").unwrap();
                probe.local_addr().unwrap().ip()
            },
            p.sock.local_addr().unwrap().port(),
        );
        let _ = p.rtc.handle_input(Input::Receive(
            Instant::now(),
            Receive {
                proto: Protocol::Udp,
                source,
                destination,
                contents: buf[..n].try_into().unwrap(),
            },
        ));
    }
    let _ = p.rtc.handle_input(Input::Timeout(Instant::now()));
    got
}

async fn connect(addr: SocketAddr, guest: &str) -> (Ws, Peer, Vec<Value>) {
    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://{addr}/voice/ws?guest={guest}"))
            .await
            .unwrap();
    let mut held = Vec::new();
    wait(&mut ws, "hello", &mut held).await;
    let (mut rtc, sock, mic, slots, offer, pending) = peer_offer();
    ws.send(Message::Text(
        json!({"t": "rtc_offer", "sdp": offer}).to_string().into(),
    ))
    .await
    .unwrap();
    let ans = wait(&mut ws, "rtc_answer", &mut held).await;
    let answer = SdpAnswer::from_sdp_string(ans["sdp"].as_str().unwrap()).unwrap();
    rtc.sdp_api().accept_answer(pending, answer).unwrap();
    (
        ws,
        Peer {
            rtc,
            sock,
            mic,
            slots,
        },
        held,
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_talker_reaches_a_listener_over_webrtc() {
    let dir = idn_common::testing::TempDir::new("voice-rtc");
    let mut cfg = VoiceConfig::new(dir.to_path_buf());
    cfg.udp = Some("0.0.0.0:0".parse().unwrap());
    let v = Voice::start(cfg, Arc::new(TestHost)).unwrap();
    let ch = v
        .hub
        .put_channel(idn_voice::model::Channel {
            id: 0,
            name: "Yard".into(),
            description: String::new(),
            kind: idn_voice::model::Kind::Walkie {
                half_duplex: false,
                limit_s: 60,
            },
            access: idn_voice::model::Access::Guest,
            record: false,
            talk_source: None,
            as_source: false,
            store_forward_s: None,
            jitter_start_ms: None,
            jitter_max_ms: None,
        })
        .unwrap();
    let app = axum::Router::new().nest("/voice", v.router());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(l, app.into_make_service_with_connect_info::<SocketAddr>())
            .await
            .unwrap();
    });
    let (mut ta, mut a, mut ha) = connect(addr, "Talker").await;
    let (mut tb, mut b, mut hb) = connect(addr, "Listener").await;
    for (ws, held) in [(&mut ta, &mut ha), (&mut tb, &mut hb)] {
        ws.send(Message::Text(
            json!({"t": "join", "channel": ch.id}).to_string().into(),
        ))
        .await
        .unwrap();
        wait(ws, "joined", held).await;
    }
    // Both connect (ICE, DTLS).
    let end = Instant::now() + Duration::from_secs(10);
    let pt = loop {
        step(&mut a);
        step(&mut b);
        let pt = a.rtc.writer(a.mic).and_then(|w| {
            w.payload_params()
                .find(|p| p.spec().codec == Codec::Opus)
                .map(|p| p.pt())
        });
        if a.rtc.is_connected()
            && b.rtc.is_connected()
            && let Some(pt) = pt
        {
            break pt;
        }
        assert!(Instant::now() < end, "WebRTC didn't connect");
    };
    ta.send(Message::Text(
        json!({"t": "talk", "on": true, "channel": ch.id})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    while wait(&mut ta, "talking", &mut ha).await["on"] != true {}
    // 50 packets of 20 ms; the listener gets them on one slot.
    let start = Instant::now();
    let mut sent = 0u64;
    let mut got: Vec<(Mid, Vec<u8>)> = Vec::new();
    let end = Instant::now() + Duration::from_secs(5);
    while got.len() < 40 && Instant::now() < end {
        if sent < 50 && start.elapsed() >= Duration::from_millis(20 * sent) {
            let data = vec![0xF8u8, (sent & 0xff) as u8, 7, 7];
            if let Some(w) = a.rtc.writer(a.mic) {
                let _ = w.write(
                    pt,
                    Instant::now(),
                    MediaTime::new(sent * 960, Frequency::FORTY_EIGHT_KHZ),
                    data,
                );
            }
            sent += 1;
        }
        step(&mut a);
        got.extend(step(&mut b));
    }
    assert!(got.len() >= 40, "{} of 50 packets", got.len());
    let slot = got[0].0;
    assert!(b.slots.contains(&slot), "on a slot");
    assert!(got.iter().all(|(m, _)| *m == slot));
    assert_eq!(got[0].1[0], 0xF8);
    assert_eq!(&got[0].1[2..], &[7, 7]);
    // The listener was told which talker the slot carries.
    let s = wait(&mut tb, "slot", &mut hb).await;
    assert_eq!(s["talker"]["name"], "Talker (guest)");
}
