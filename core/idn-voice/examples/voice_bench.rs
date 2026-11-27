//! IDN Voice under load (profile round 8): the server in one process, guests in another,
//! over the WebSocket path (the forwarding and recording are the same for WebRTC).
//!
//! `voice_bench server PORT DIR` serves Voice on 127.0.0.1:PORT (2 worker threads, as the
//! IDN Site Manager's) with two guest channels: 1 "Load" (full duplex) and 2 "Half" (half
//! duplex), both recorded; prints `ready`.
//!
//! `voice_bench load PORT CHANNEL LISTENERS TALKERS SECONDS` joins that many guests,
//! the talkers each sending a 60-byte packet every 20 ms (its first 8 bytes the time it
//! was sent), and prints what arrived and how late: one line of `key=value`.

use futures_util::{SinkExt, StreamExt};
use idn_voice::model::{Access, Channel, Kind};
use idn_voice::{Host, User, Voice, VoiceConfig};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

struct BenchHost;

impl Host for BenchHost {
    fn session_user(&self, _: &axum::http::HeaderMap) -> Option<User> {
        None
    }
    fn login(&self, _: &str, _: &str, _: std::net::IpAddr) -> Option<User> {
        None
    }
    fn users(&self) -> Vec<User> {
        Vec::new()
    }
    fn audit(&self, _: &str, _: &str, _: Value) {}
}

fn now_us() -> u64 {
    idn_common::epoch::now().as_micros() as u64
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    match a.first().map(String::as_str) {
        Some("server") if a.len() == 3 => server(a[1].parse().unwrap(), &a[2]),
        Some("load") if a.len() == 6 => load(
            a[1].parse().unwrap(),
            a[2].parse().unwrap(),
            a[3].parse().unwrap(),
            a[4].parse().unwrap(),
            a[5].parse().unwrap(),
        ),
        _ => {
            eprintln!("voice_bench server PORT DIR | load PORT CHANNEL LISTENERS TALKERS SECONDS");
            std::process::exit(2);
        }
    }
}

fn server(port: u16, dir: &str) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let _ = std::fs::remove_dir_all(dir);
        let mut cfg = VoiceConfig::new(dir.into());
        cfg.udp = None;
        let v = Voice::start(cfg, Arc::new(BenchHost)).unwrap();
        for (name, half) in [("Load", false), ("Half", true)] {
            v.hub
                .put_channel(Channel {
                    id: 0,
                    name: name.into(),
                    description: String::new(),
                    kind: Kind::Walkie {
                        half_duplex: half,
                        limit_s: 0,
                    },
                    access: Access::Guest,
                    record: true,
                    talk_source: None,
                    as_source: false,
                    store_forward_s: None,
                    jitter_start_ms: None,
                    jitter_max_ms: None,
                })
                .unwrap();
        }
        let app = axum::Router::new().nest("/voice", v.router());
        use axum::serve::ListenerExt;
        let l = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .unwrap()
            .tap_io(|tcp| {
                let _ = tcp.set_nodelay(true);
            });
        println!("ready");
        axum::serve(
            l,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
}

/// Latencies in µs, and counts.
#[derive(Default)]
struct Got {
    lat: std::sync::Mutex<Vec<u32>>,
    frames: AtomicU64,
    busy: AtomicU64,
}

fn load(port: u16, channel: u32, listeners: usize, talkers: usize, secs: u64) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let got = Arc::new(Got::default());
    let sent = Arc::new(AtomicU64::new(0));
    let go = Arc::new(AtomicBool::new(false));
    rt.block_on(async {
        let mut tasks = Vec::new();
        let joined = Arc::new(AtomicU64::new(0));
        for i in 0..listeners + talkers {
            let talker = i >= listeners;
            let (got, sent, go, joined) = (got.clone(), sent.clone(), go.clone(), joined.clone());
            tasks.push(tokio::spawn(async move {
                let url = format!("ws://127.0.0.1:{port}/voice/ws?guest=g{i}");
                // (Browsers send WebSocket messages at once: no Nagle here either.)
                let (ws, _) = tokio_tungstenite::connect_async_with_config(url, None, true)
                    .await
                    .expect("connect");
                let (mut tx, mut rx) = ws.split();
                let j = json!({"t": "join", "channel": channel}).to_string();
                tx.send(Message::Text(j.into())).await.unwrap();
                let mut joined_here = false;
                // Reader: frames' lateness; joined, talking and busy.
                let (talk_tx, mut talk_rx) = tokio::sync::mpsc::channel::<bool>(4);
                let reader = {
                    let got = got.clone();
                    tokio::spawn(async move {
                        while let Some(Ok(m)) = rx.next().await {
                            match m {
                                Message::Binary(b) if b.len() >= 21 => {
                                    let ts = u64::from_le_bytes(b[13..21].try_into().unwrap());
                                    let late = now_us().saturating_sub(ts).min(u32::MAX as u64);
                                    got.frames.fetch_add(1, Ordering::Relaxed);
                                    got.lat.lock().unwrap().push(late as u32);
                                }
                                Message::Text(t) => {
                                    let v: Value = serde_json::from_str(&t).unwrap_or_default();
                                    match v["t"].as_str() {
                                        Some("joined") => {
                                            let _ = talk_tx.send(true).await;
                                        }
                                        Some("busy") => {
                                            got.busy.fetch_add(1, Ordering::Relaxed);
                                        }
                                        _ => {}
                                    }
                                }
                                _ => {}
                            }
                        }
                    })
                };
                if talk_rx.recv().await == Some(true) {
                    joined_here = true;
                    joined.fetch_add(1, Ordering::Relaxed);
                }
                while !go.load(Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                if talker && joined_here {
                    let t = json!({"t": "talk", "on": true, "channel": channel}).to_string();
                    tx.send(Message::Text(t.into())).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let end = Instant::now() + Duration::from_secs(secs);
                    let mut tick = tokio::time::interval(Duration::from_millis(20));
                    while Instant::now() < end {
                        tick.tick().await;
                        let mut b = vec![1u8];
                        b.extend_from_slice(&now_us().to_le_bytes());
                        b.resize(61, 0x55);
                        if tx.send(Message::Binary(b.into())).await.is_err() {
                            break;
                        }
                        sent.fetch_add(1, Ordering::Relaxed);
                    }
                    let t = json!({"t": "talk", "on": false}).to_string();
                    let _ = tx.send(Message::Text(t.into())).await;
                } else {
                    tokio::time::sleep(Duration::from_secs(secs) + Duration::from_millis(100))
                        .await;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
                let _ = tx.close().await;
                reader.abort();
            }));
        }
        let all = (listeners + talkers) as u64;
        let t0 = Instant::now();
        while joined.load(Ordering::Relaxed) < all && t0.elapsed() < Duration::from_secs(30) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        eprintln!(
            "joined {} of {all} in {} ms",
            joined.load(Ordering::Relaxed),
            t0.elapsed().as_millis()
        );
        go.store(true, Ordering::Relaxed);
        for t in tasks {
            let _ = t.await;
        }
    });
    let mut lat = got.lat.lock().unwrap().clone();
    lat.sort_unstable();
    let pct = |p: f64| {
        lat.get(((lat.len() as f64 * p) as usize).min(lat.len().saturating_sub(1)))
            .copied()
            .unwrap_or(0)
    };
    let sent = sent.load(Ordering::Relaxed);
    // Every packet goes to every other member (full duplex); in half duplex to all but
    // the one talker.
    let members = (listeners + talkers) as u64;
    let expected = sent * (members - 1);
    println!(
        "listeners={listeners} talkers={talkers} sent={sent} received={} expected={expected} busy={} p50_us={} p99_us={} max_us={}",
        got.frames.load(Ordering::Relaxed),
        got.busy.load(Ordering::Relaxed),
        pct(0.5),
        pct(0.99),
        lat.last().copied().unwrap_or(0)
    );
}
