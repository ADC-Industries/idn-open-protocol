//! Receiving what is routed: the [`Router`], one input per transmitter.

use super::*;
use idn_common::backoff::Backoff;
use idn_common::sync::LockOk as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---------------------------------------------------------------------------------------
// Receiving what is routed.

/// What a [`Router`] hands on.
pub enum RouterEvent<'a> {
    /// A block of sound from a transmitter.
    Block(RoutedBlock<'a>),
    /// A transmitter is no longer in any route: forget what was kept for it.
    Gone(&'a str),
}

/// A block of sound from one transmitter, with where its channels go.
pub struct RoutedBlock<'a> {
    /// The transmitter (`HOST (name)`).
    pub tx: &'a str,
    /// The block as received (it may hold only some of the transmitter's channels).
    pub audio: &'a crate::input::AudioIn,
    /// (index into `audio.channels`, receiver channel) for every route the block
    /// serves; a block channel may feed several receiver channels.
    pub map: &'a [(usize, u16)],
    /// The receiver's latency the site set (§A.12), in ms: the lane playing this
    /// transmitter should hold it (`Lane::follow_site`); `None`: the program's own.
    pub latency_ms: Option<u32>,
    /// What lies between the transmitter and this machine (R.30), as the system's routes
    /// say once it connected: where an adapting lane starts ([`Path::start_ms`]).
    ///
    /// [`Path::start_ms`]: idn_common::net::Path::start_ms
    pub path: idn_common::net::Path,
}

/// How a [`Router`] receives.
#[derive(Clone)]
pub struct RouterConfig {
    /// Lossless float sound from IDN transmitters (`audio-f32/1`, twice the bandwidth).
    /// On by default.
    pub lossless: bool,
}

impl Default for RouterConfig {
    fn default() -> Self {
        RouterConfig { lossless: true }
    }
}

type Callback = Arc<Mutex<dyn FnMut(RouterEvent) + Send>>;

/// What one transmitter's input has seen.
#[derive(Default)]
struct TxSeen {
    last_audio: Option<std::time::Instant>,
    /// Lost (unreachable, refused), until it connects again.
    lost: bool,
    /// The transmitter's channel count, from the latest block.
    channels: Option<usize>,
    since: Option<std::time::Instant>,
    /// When each receiver channel routed from it last had sound (above −60 dBFS): the
    /// matrix's "signal arriving" (§A.15).
    heard: BTreeMap<u16, std::time::Instant>,
    /// The transport the latest block came by ([`AudioIn::via`](crate::input::AudioIn::via)):
    /// `machine` on this machine's ring, else the network's (§A.16).
    via: &'static str,
}

/// Sound above this (−60 dBFS) counts as a signal arriving on a route (§A.15).
const SIGNAL: f32 = 0.001;

/// One transmitter's input and its thread.
struct TxLane {
    input: Arc<crate::input::Input>,
    /// The transmitter channels asked for, ascending.
    wanted: Vec<u16>,
    /// (transmitter channel, receiver channel).
    map: Arc<Mutex<Vec<(u16, u16)>>>,
    seen: Arc<Mutex<TxSeen>>,
    /// Its thread, stopped and waited for with it.
    _worker: idn_common::worker::Worker,
}

/// Receives what is routed to an [`AudioReceiver`] (idn-routing.md §A.7): one input per
/// transmitter in its routes, sound only, the needed channels only, encrypted with the
/// machine's site identity; each block goes to the callback with its channel map, and
/// each route's state goes to the Node. Stops when dropped.
pub struct Router {
    /// Its thread, stopped and waited for with it.
    _worker: idn_common::worker::Worker,
}

impl Router {
    /// Starts receiving for `rx`, through `node`'s identity, calling `on_event` from the
    /// router's threads (one at a time).
    pub fn start(
        node: Arc<super::super::Node>,
        rx: Arc<AudioReceiver>,
        cfg: RouterConfig,
        on_event: impl FnMut(RouterEvent) + Send + 'static,
    ) -> io::Result<Router> {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cb: Callback = Arc::new(Mutex::new(on_event));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("idn-router".into())
                .spawn(move || route_loop(node, rx, cfg, cb, stop))?
        };
        Ok(Router {
            _worker: idn_common::worker::Worker::from_parts(stop, thread),
        })
    }
}

fn route_loop(
    node: Arc<super::super::Node>,
    rx: Arc<AudioReceiver>,
    cfg: RouterConfig,
    cb: Callback,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;
    use std::time::Instant;
    let mut lanes: BTreeMap<String, TxLane> = BTreeMap::new();
    // Transmitters whose lane wouldn't open: tried again after a wait that doubles (to
    // 30 s), logged when it first fails (every 250 ms, and a line each).
    let mut failing: BTreeMap<String, (std::time::Instant, Backoff)> = BTreeMap::new();
    let mut routes = rx.routes();
    let mut last_report: Option<Vec<(u16, RouteState, bool, &'static str)>> = None;
    let mut last_need: Option<u32> = None;
    let mut reported_at = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        // Inputs to match the routes: opened, re-asked for channels, or closed.
        let mut want: BTreeMap<String, Vec<(u16, u16)>> = BTreeMap::new();
        for r in routes.iter().filter(|r| r.ch < rx.channels()) {
            want.entry(r.tx.clone()).or_default().push((r.tx_ch, r.ch));
        }
        let gone: Vec<String> = lanes
            .keys()
            .filter(|t| !want.contains_key(*t))
            .cloned()
            .collect();
        for t in gone {
            lanes.remove(&t);
            (cb.lock_ok())(RouterEvent::Gone(&t));
        }
        for (tx, map) in &want {
            let mut wanted: Vec<u16> = map.iter().map(|(t, _)| *t).collect();
            wanted.sort();
            wanted.dedup();
            match lanes.get_mut(tx) {
                Some(l) => {
                    let mut m = l.map.lock_ok();
                    if *m != *map {
                        *m = map.clone();
                    }
                    drop(m);
                    if l.wanted != wanted {
                        l.input.set_audio_channels(Some(wanted.clone()));
                        l.wanted = wanted;
                    }
                }
                None if failing
                    .get(tx)
                    .is_some_and(|(at, b)| b.current().is_some_and(|w| at.elapsed() < w)) => {}
                None => match open_lane(&node, &rx, tx, wanted, map.clone(), &cfg, cb.clone()) {
                    Ok(l) => {
                        failing.remove(tx);
                        lanes.insert(tx.clone(), l);
                    }
                    Err(e) => {
                        if !failing.contains_key(tx) {
                            crate::log::log(&format!("audio route from {tx}: {e}"));
                        }
                        let (at, b) = failing.entry(tx.clone()).or_insert_with(|| {
                            let b =
                                Backoff::new(Duration::from_millis(500), Duration::from_secs(30));
                            (std::time::Instant::now(), b)
                        });
                        b.next();
                        *at = std::time::Instant::now();
                    }
                },
            }
        }
        // Each routed channel's state, to the Node when it changes, and every 2 s (a Node
        // that restarted knows none until then).
        let now = Instant::now();
        // A signal: sound on the route in the last half second (§A.15; with the report
        // every 250 ms and the pages' refresh, a ring goes about 1–2 s after the sound).
        let states: Vec<(u16, RouteState, bool, &'static str)> = routes
            .iter()
            .map(|r| {
                let mut sig = false;
                let mut via = "";
                let st = if r.ch >= rx.channels() {
                    RouteState::NoChannel
                } else {
                    match lanes.get(&r.tx).map(|l| l.seen.lock_ok()) {
                        None => RouteState::NoSource,
                        Some(s) => {
                            via = s.via;
                            sig = s.heard.get(&r.ch).is_some_and(|t| {
                                now.duration_since(*t) < Duration::from_millis(500)
                            });
                            let fresh = s
                                .last_audio
                                .is_some_and(|t| now.duration_since(t) < Duration::from_secs(2));
                            let waited = s
                                .since
                                .is_some_and(|t| now.duration_since(t) > Duration::from_secs(5));
                            if s.channels.is_some_and(|n| r.tx_ch as usize >= n) {
                                RouteState::NoChannel
                            } else if fresh {
                                RouteState::Ok
                            } else if s.lost || waited {
                                RouteState::NoSource
                            } else {
                                RouteState::Connecting
                            }
                        }
                    }
                };
                let via = if st == RouteState::Ok { via } else { "" };
                (r.ch, st, sig && st == RouteState::Ok, via)
            })
            .collect();
        // The attached mixer: the site's floor applied, its need told with the states
        // (when it changed by 2 ms or more, and every 2 s).
        let need = rx.sync_mixer();
        let need_moved = match (need, last_need) {
            (Some(a), Some(b)) => a.abs_diff(b) >= 2,
            (a, b) => a.is_some() != b.is_some(),
        };
        if last_report.as_ref() != Some(&states)
            || need_moved
            || reported_at.elapsed() >= Duration::from_secs(2)
        {
            rx.report_need(&states, need);
            last_report = Some(states);
            last_need = need;
            reported_at = now;
        }
        if let Some(r) = rx.wait_change(Duration::from_millis(250)) {
            routes = r;
        }
    }
}

fn open_lane(
    node: &Arc<super::super::Node>,
    rx: &Arc<AudioReceiver>,
    tx: &str,
    wanted: Vec<u16>,
    map: Vec<(u16, u16)>,
    cfg: &RouterConfig,
    cb: Callback,
) -> io::Result<TxLane> {
    use crate::input::{ConnectionState, Input, InputConfig, InputEvent};
    use std::sync::atomic::{AtomicBool, Ordering};
    let input = Arc::new(Input::open(InputConfig {
        receive_video: false,
        audio_channels: Some(wanted.clone()),
        lossless_audio: cfg.lossless,
        secure: Some(node.secure_config()),
        link: Some(node.link_config()),
        ..InputConfig::new(tx)
    })?);
    let map = Arc::new(Mutex::new(map));
    let seen = Arc::new(Mutex::new(TxSeen {
        since: Some(std::time::Instant::now()),
        ..Default::default()
    }));
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let (input, map, seen, stop, tx) = (
            input.clone(),
            map.clone(),
            seen.clone(),
            stop.clone(),
            tx.to_owned(),
        );
        let latency = rx.latency.clone();
        std::thread::Builder::new()
            .name("idn-router-rx".into())
            .spawn(move || {
                let mut block_map: Vec<(usize, u16)> = Vec::new();
                // The routed channels with sound in a block (reused).
                let mut loud: Vec<u16> = Vec::new();
                // With IDN_ROUTING_LOG=1, every 10 s while sound flows, a line on what
                // arrives: the source channels the blocks hold, where they go, and each
                // one's peak.
                let note = idn_common::switches::on("IDN_ROUTING_LOG");
                let (mut next_note, mut peaks) = (std::time::Instant::now(), Vec::<f32>::new());
                // Real-time once the site asks for 5 ms or less (§A.12): arrivals handed to
                // the lane late eat a buffer that small.
                let mut rt = false;
                // The path to the transmitter, once connected (looked up once a connection).
                let mut path = idn_common::net::Path::Unknown;
                while !stop.load(Ordering::Relaxed) {
                    // (And on this machine or a wire, where adapting lanes run small buffers
                    // too: R.30 step 4.)
                    let small = (1..=5).contains(&latency.load(Ordering::Relaxed))
                        || matches!(path, idn_common::net::Path::Local | idn_common::net::Path::Wired);
                    if !rt && small {
                        rt = true;
                        let got = idn_common::rt::audio_this_thread();
                        crate::log::log(&format!(
                            "audio route from {tx} ({}): real-time thread {}",
                            path.name(),
                            if got { "on" } else { "not allowed" }
                        ));
                    }
                    match input.recv(Duration::from_millis(100)) {
                        Some(InputEvent::Audio(a)) => {
                            // Which source channel each block channel is (§E.21.3).
                            let (held, of) = match crate::ext::parse_channels_metadata(a.header.metadata.as_deref()) {
                                Some((l, of)) => (Some(l), Some(of)),
                                None => (None, None),
                            };
                            block_map.clear();
                            // (Locked once a block, not once a channel.)
                            let map = map.lock_ok();
                            for (i, _) in a.channels.iter().enumerate() {
                                let src = match &held {
                                    Some(l) => match l.get(i) {
                                        Some(&c) => c,
                                        None => continue,
                                    },
                                    None => i as u16,
                                };
                                for &(t, r) in map.iter() {
                                    if t == src {
                                        block_map.push((i, r));
                                    }
                                }
                            }
                            drop(map);
                            {
                                // Which routed channels have sound (stopping at the first
                                // loud sample of each; a source channel routed to several
                                // looked at once: the map holds each one's together), before
                                // taking the lock.
                                let mut prev: Option<(usize, bool)> = None;
                                loud.clear();
                                for &(i, r) in &block_map {
                                    let l = match prev {
                                        Some((p, l)) if p == i => l,
                                        _ => {
                                            let l = a.channels[i].iter().any(|v| v.abs() > SIGNAL);
                                            prev = Some((i, l));
                                            l
                                        }
                                    };
                                    if l {
                                        loud.push(r);
                                    }
                                }
                                let now = std::time::Instant::now();
                                let mut s = seen.lock_ok();
                                s.last_audio = Some(now);
                                s.lost = false;
                                s.via = a.via;
                                s.channels = of.or(held.is_none().then_some(a.channels.len()));
                                for &r in &loud {
                                    s.heard.insert(r, now);
                                }
                            }
                            peaks.resize(if note { a.channels.len() } else { 0 }, 0.0);
                            for (p, c) in peaks.iter_mut().zip(&a.channels) {
                                *p = c.iter().fold(*p, |m, v| m.max(v.abs()));
                            }
                            if note && std::time::Instant::now() >= next_note {
                                crate::log::log(&format!(
                                    "routing: {tx}: block holds source channels {} of {}, to receiver channels {:?}, peaks {:?}",
                                    held.as_ref().map_or("all".to_owned(), |l| crate::ext::channel_list(l)),
                                    of.unwrap_or(a.channels.len()),
                                    block_map,
                                    peaks.iter().map(|p| (p * 1000.0).round() / 1000.0).collect::<Vec<_>>()
                                ));
                                peaks.clear();
                                next_note = std::time::Instant::now() + Duration::from_secs(10);
                            }
                            if !block_map.is_empty() {
                                (cb.lock_ok())(RouterEvent::Block(RoutedBlock {
                                    tx: &tx,
                                    audio: &a,
                                    map: &block_map,
                                    latency_ms: Some(latency.load(Ordering::Relaxed))
                                        .filter(|&ms| ms > 0),
                                    path,
                                }));
                            }
                        }
                        Some(InputEvent::State(ConnectionState::Lost(_))) => {
                            seen.lock_ok().lost = true;
                        }
                        Some(InputEvent::State(
                            ConnectionState::Connected(addr) | ConnectionState::Routed { addr, .. },
                        )) => {
                            seen.lock_ok().lost = false;
                            path = idn_common::net::path_to(addr.ip());
                            if note {
                                crate::log::log(&format!("routing: {tx}: path {}", path.name()));
                            }
                        }
                        _ => {}
                    }
                }
            })?
    };
    Ok(TxLane {
        input,
        wanted,
        map,
        seen,
        _worker: idn_common::worker::Worker::from_parts(stop, thread),
    })
}
