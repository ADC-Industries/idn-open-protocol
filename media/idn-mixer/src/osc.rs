//! OSC control (spec/extensions/idn-mixer.md §M.16): Open Sound Control 1.0 over UDP, for
//! tablet apps (TouchOSC, Open Stage Control), show controllers and scripts.
//!
//! An address is a path into the desk's settings, from 1: `/ch/3/fader -6.0`,
//! `/ch/3/eq/2/gain 3.0`, `/aux/1/mute 1`, `/ch/5/sends/2/level -10`. A value takes the
//! setting's own type (an on/off setting takes 0/1, T/F or a float a button sends); a
//! fader or send level also has `/travel` (0…1 of the panel's fader law). An address
//! with no arguments is a query: the reply has every value under it. `/subscribe`
//! (again within 10 s to stay) sends back each change made anywhere; `/subscribe
//! "meters"` the meters as well, ten times a second. `/ch/N/preamp/gain`, `…/phantom`
//! and `…/invert` are the X32 preamp behind channel N (§M.23.1: the `preamp` command, not
//! a desk setting; `invert` is the X32 polarity, on the X32's own channel strip).
//!
//! [`decode`] and [`encode`] are the wire format (tested on their own); [`Osc`] serves a
//! [`Console`] on a UDP port.

use crate::engine::Console;
use crate::surface::{db_to_travel, travel_to_db};
use idn_common::sync::LockOk as _;
use serde_json::{Map, Value, json};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub use idn_open_protocol::osc::{Arg, Message, PACKET, decode, encode, packets};

/// How long a subscription lasts without being renewed.
pub const SUBSCRIPTION: Duration = Duration::from_secs(10);

/// A setting as an OSC argument: on/off as 0/1 (what most controls take), whole
/// numbers as ints, the rest as floats.
fn arg_of(v: &Value) -> Option<Arg> {
    Some(match v {
        Value::Bool(b) => Arg::Int(*b as i32),
        Value::Number(n) if n.is_i64() || n.is_u64() => {
            Arg::Int(n.as_i64().unwrap_or(i64::MAX).min(i32::MAX as i64) as i32)
        }
        Value::Number(n) => Arg::Float(n.as_f64()? as f32),
        Value::String(s) => Arg::Str(s.clone()),
        _ => return None,
    })
}

/// Settings not reachable by OSC: plugin states (large, and set by the plugin) and
/// what the desk keeps for itself.
const HIDDEN: &[&str] = &["inserts", "state", "shown"];

/// The desk's places in the settings: an address's first parts, the settings' key, and
/// whether it is numbered.
const ROOTS: &[(&str, &str, bool)] = &[
    ("ch", "channels", true),
    ("main", "main", false),
    ("aux", "aux", true),
    ("group", "groups", true),
    ("matrix", "matrix", true),
    ("fx", "fx", true),
    ("headphones", "monitor", false),
    ("dca", "dca", true),
    ("mutegroup", "mute_groups", true),
    ("talkback", "talkback", false),
];

/// Desk-wide settings under `/desk/`.
const DESK: &[&str] = &[
    "solo_afl",
    "auto_park",
    "automix",
    "duckers",
    "afv",
    "output_delay",
];

/// Every value OSC can reach, by address, as the settings are now.
pub fn flatten(settings: &Value) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for &(osc, key, numbered) in ROOTS {
        match (&settings[key], numbered) {
            (Value::Array(items), true) => {
                for (i, v) in items.iter().enumerate() {
                    walk(&format!("/{osc}/{}", i + 1), v, &mut out);
                }
            }
            (v, false) => walk(&format!("/{osc}"), v, &mut out),
            _ => {}
        }
    }
    for k in DESK {
        walk(&format!("/desk/{k}"), &settings[*k], &mut out);
    }
    out
}

fn walk(addr: &str, v: &Value, out: &mut Vec<(String, Value)>) {
    match v {
        Value::Object(o) => {
            for (k, v) in o {
                if !HIDDEN.contains(&k.as_str()) {
                    walk(&format!("{addr}/{k}"), v, out);
                }
            }
        }
        Value::Array(a) => {
            for (i, v) in a.iter().enumerate() {
                walk(&format!("{addr}/{}", i + 1), v, out);
            }
        }
        Value::Null => {}
        v => {
            out.push((addr.to_owned(), v.clone()));
            // A level in dB also as the fader's travel.
            if let Some(d) = v.as_f64().filter(|_| is_level(addr)) {
                out.push((format!("{addr}/travel"), json!(db_to_travel(d as f32))));
            }
        }
    }
}

fn is_level(addr: &str) -> bool {
    addr.ends_with("/fader") || addr.ends_with("/level")
}

/// What an address changes: the API command, its addressing (`{"n": 3}`), and the
/// path below that (`["eq", "2", "gain"]`).
type Target<'a> = (&'static str, Map<String, Value>, Vec<&'a str>);

fn target<'a>(parts: &[&'a str]) -> Result<Target<'a>, String> {
    let num = |i: usize| -> Result<u64, String> {
        parts
            .get(i)
            .and_then(|n| n.parse().ok())
            .filter(|&n| n >= 1)
            .ok_or_else(|| format!("/{}: a number from 1 next", parts[0]))
    };
    let mut m = Map::new();
    let (cmd, used) = match parts.first().copied().unwrap_or("") {
        "ch" => {
            m.insert("n".into(), json!(num(1)?));
            ("channel", 2)
        }
        "main" => {
            m.insert("bus".into(), json!("main"));
            ("bus", 1)
        }
        "aux" => {
            m.insert("bus".into(), json!(num(1)?));
            ("bus", 2)
        }
        "headphones" => {
            m.insert("bus".into(), json!("headphones"));
            ("bus", 1)
        }
        "group" => {
            m.insert("group".into(), json!(num(1)?));
            ("group", 2)
        }
        "matrix" => {
            m.insert("matrix".into(), json!(num(1)?));
            ("matrix", 2)
        }
        "fx" => {
            m.insert("fx".into(), json!(num(1)?));
            ("fx", 2)
        }
        "dca" => {
            m.insert("dca".into(), json!(num(1)?));
            ("dca", 2)
        }
        "mutegroup" => {
            m.insert("group".into(), json!(num(1)?));
            ("mute_group", 2)
        }
        "talkback" => ("talkback", 1),
        "desk" if parts.get(1).is_some_and(|k| DESK.contains(k)) => ("desk", 1),
        other => return Err(format!("no OSC address /{other}…")),
    };
    Ok((cmd, m, parts[used..].to_vec()))
}

/// The value a setting takes from an argument, by the setting's type now.
fn value_for(now: &Value, arg: &Arg, travel: bool) -> Result<Value, String> {
    // (Not NaN or infinite: JSON has no such number, it became null, and the merge then
    // dropped the setting — a fader back at unity; deep bug run 2026-11-09.)
    let num = || {
        match arg {
            Arg::Int(i) => Some(*i as f64),
            Arg::Float(f) => Some(*f as f64),
            Arg::Bool(b) => Some(*b as u8 as f64),
            Arg::Str(s) => s.trim().parse().ok(),
            Arg::Nil => None,
        }
        .filter(|x: &f64| x.is_finite())
    };
    Ok(match now {
        Value::Bool(_) => Value::Bool(match arg {
            Arg::Bool(b) => *b,
            Arg::Int(i) => *i != 0,
            Arg::Float(f) => *f >= 0.5,
            Arg::Str(s) => idn_common::text::parse_on_off(s) == Some(true),
            Arg::Nil => return Err("on/off: 1 or 0".into()),
        }),
        Value::Number(n) => {
            let x = num().ok_or("a number")?;
            if travel {
                json!(travel_to_db(x.clamp(0.0, 1.0) as f32))
            } else if n.is_f64() {
                json!(x)
            } else {
                json!(x.round().max(0.0) as u64)
            }
        }
        Value::String(_) => match arg {
            Arg::Str(s) => json!(s),
            Arg::Nil => return Err("a string".into()),
            _ => json!(num().unwrap_or(0.0).to_string()),
        },
        // Unset (an optional setting): as the argument comes.
        Value::Null => match arg {
            Arg::Bool(b) => json!(b),
            Arg::Int(i) => json!(i),
            Arg::Float(f) => json!(f),
            Arg::Str(s) => json!(s),
            Arg::Nil => return Err("a value".into()),
        },
        _ => return Err("this address holds several settings: address one of them".into()),
    })
}

/// A merge patch setting `path` (from 1 in arrays; nulls keep the others) to `v`.
fn nest(path: &[&str], v: Value) -> Value {
    path.iter().rev().fold(v, |v, p| match p.parse::<usize>() {
        Ok(i) if i >= 1 => {
            let mut a = vec![Value::Null; i];
            a[i - 1] = v;
            Value::Array(a)
        }
        _ => json!({ *p: v }),
    })
}

/// What a message does to the desk: the command and its body. `Ok(None)`: a query.
pub fn command_for(msg: &Message, settings: &Value) -> Result<Option<(String, Value)>, String> {
    let parts: Vec<&str> = msg
        .addr
        .split('/')
        .skip(1)
        .filter(|p| !p.is_empty())
        .collect();
    let first = msg.args.first();
    let text = || match first {
        Some(Arg::Str(s)) => Ok(s.clone()),
        _ => Err(format!("{}: a name", msg.addr)),
    };
    // Actions.
    match parts.as_slice() {
        ["snapshot", "recall"] => {
            return Ok(Some(("snapshot_recall".into(), json!({"name": text()?}))));
        }
        ["snapshot", "save"] => {
            return Ok(Some(("snapshot_save".into(), json!({"name": text()?}))));
        }
        ["button", n, "press"] => {
            // A button's release (0) does nothing.
            if matches!(first, Some(Arg::Int(0)) | Some(Arg::Bool(false)))
                || matches!(first, Some(Arg::Float(f)) if *f < 0.5)
            {
                return Ok(Some(("".into(), Value::Null)));
            }
            let n: u64 = n.parse().map_err(|_| "/button/N/press")?;
            return Ok(Some(("press".into(), json!({"button": n}))));
        }
        // An X32 preamp behind a channel (§M.23.1): gain (dB), phantom, invert (the X32
        // polarity); not a desk setting, so the `preamp` command.
        ["ch", n, "preamp", what @ ("gain" | "phantom" | "invert")] if first.is_some() => {
            let n: u64 = n
                .parse()
                .ok()
                .filter(|&n| n >= 1)
                .ok_or("/ch/N/preamp: a channel from 1")?;
            let arg = first.unwrap_or(&Arg::Nil);
            let body = if *what == "gain" {
                json!({"n": n, "gain_db": value_for(&json!(0.0), arg, false)?})
            } else {
                json!({"n": n, *what: value_for(&json!(false), arg, false)?})
            };
            return Ok(Some(("preamp".into(), body)));
        }
        ["clear_solo"] => return Ok(Some(("clear_solo".into(), json!({})))),
        ["loudness_reset"] => return Ok(Some(("loudness_reset".into(), json!({})))),
        ["record", "start"] => return Ok(Some(("record_start".into(), json!({})))),
        ["record", "stop"] => return Ok(Some(("record_stop".into(), json!({})))),
        // Any API command, its body as JSON text.
        ["cmd"] => {
            let cmd = text()?;
            // Network routes and receivers' latency act on this machine's other programs
            // through its IDN Node: from the desk's panel only (OSC has no password).
            if matches!(cmd.as_str(), "route" | "receiver_latency") {
                return Err(format!("/cmd {cmd}: from the desk's panel only"));
            }
            let body = match msg.args.get(1) {
                Some(Arg::Str(b)) => {
                    serde_json::from_str(b).map_err(|e| format!("/cmd: bad JSON: {e}"))?
                }
                _ => json!({}),
            };
            return Ok(Some((cmd, body)));
        }
        _ => {}
    }
    if first.is_none() {
        return Ok(None);
    }
    let (cmd, mut addressing, mut path) = target(&parts)?;
    let travel = path.last() == Some(&"travel");
    if travel {
        path.pop();
    }
    if path.is_empty() || HIDDEN.iter().any(|h| path.contains(h)) {
        return Err(format!("{}: not a setting", msg.addr));
    }
    if travel && !is_level(&format!("/{}", path.join("/"))) {
        return Err(format!("{}: travel is for faders and levels", msg.addr));
    }
    // The setting now, for its type (unset, an optional one, is null).
    let mut pointer = String::new();
    match ROOTS.iter().find(|r| r.0 == parts[0]) {
        Some(&(_, key, true)) => {
            pointer = format!("/{key}/{}", parts[1].parse::<usize>().unwrap_or(1) - 1)
        }
        Some(&(_, key, false)) => pointer = format!("/{key}"),
        None => {}
    }
    for p in &path {
        match p.parse::<usize>() {
            Ok(i) if i >= 1 => pointer += &format!("/{}", i - 1),
            _ => pointer += &format!("/{p}"),
        }
    }
    let now = settings
        .pointer(&pointer)
        .ok_or_else(|| format!("{}: no such setting", msg.addr))?;
    let v = value_for(now, first.unwrap_or(&Arg::Nil), travel)?;
    let patch = nest(&path, v);
    if let Value::Object(p) = patch {
        addressing.extend(p);
    }
    Ok(Some((cmd.into(), Value::Object(addressing))))
}

/// The replies to a query: every value under the address.
pub fn query(addr: &str, settings: &Value) -> Vec<Message> {
    let addr = addr.trim_end_matches('/');
    flatten(settings)
        .into_iter()
        .filter(|(a, _)| a == addr || a.starts_with(&format!("{addr}/")))
        .filter_map(|(a, v)| Some(Message::new(a, vec![arg_of(&v)?])))
        .collect()
}

struct Subscriber {
    to: SocketAddr,
    until: Instant,
    meters: bool,
}

struct Shared {
    console: Arc<Console>,
    socket: UdpSocket,
    /// The senders listened to; empty: any.
    allow: Vec<IpAddr>,
    subscribers: Mutex<Vec<Subscriber>>,
    stop: AtomicBool,
    /// The desk's settings as JSON, by the desk's version: made again only when it
    /// changed (every message serialised all of it).
    cached: Mutex<Option<(u64, Arc<Value>)>>,
}

/// Subscribers at most (each new one is sent the whole desk).
const MAX_SUBSCRIBERS: usize = 16;

/// A sender on this network: loopback, private, link-local (IPv4), unique-local or
/// link-local (IPv6). With no allow list, only these are listened to: OSC has no
/// password, and an answer to a forged address reflects at it.
fn on_lan(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_loopback() || v.is_private() || v.is_link_local(),
        IpAddr::V6(v) => {
            v.is_loopback()
                || (v.segments()[0] & 0xfe00) == 0xfc00
                || (v.segments()[0] & 0xffc0) == 0xfe80
                || v.to_ipv4_mapped().is_some_and(|m| on_lan(IpAddr::V4(m)))
        }
    }
}

impl Shared {
    fn send(&self, to: SocketAddr, msgs: &[Message]) {
        for p in packets(msgs) {
            let _ = self.socket.send_to(&p, to);
        }
    }

    fn settings(&self) -> Arc<Value> {
        let version = self.console.version();
        let mut c = self.cached.lock_ok();
        if let Some((v, s)) = c.as_ref()
            && *v == version
        {
            return s.clone();
        }
        let s = Arc::new(serde_json::to_value(self.console.settings()).unwrap_or(Value::Null));
        *c = Some((version, s.clone()));
        s
    }

    /// The X32 preamps' values under `addr` (§M.23.1; not in the settings).
    fn preamps(&self, addr: &str) -> Vec<Message> {
        let addr = addr.trim_end_matches('/');
        self.console
            .preamp_osc()
            .into_iter()
            .filter(|(a, _)| a == addr || a.starts_with(&format!("{addr}/")))
            .filter_map(|(a, v)| Some(Message::new(a, vec![arg_of(&v)?])))
            .collect()
    }

    /// One message from `from`.
    fn handle(&self, msg: &Message, from: SocketAddr) {
        match msg.addr.as_str() {
            "/subscribe" => {
                let meters = matches!(msg.args.first(), Some(Arg::Str(s)) if s == "meters");
                let mut subs = self.subscribers.lock_ok();
                let fresh = !subs.iter().any(|s| s.to == from);
                if fresh && subs.len() >= MAX_SUBSCRIBERS {
                    return;
                }
                subs.retain(|s| s.to != from);
                subs.push(Subscriber {
                    to: from,
                    until: Instant::now() + SUBSCRIPTION,
                    meters,
                });
                drop(subs);
                // A new subscriber gets everything, so it starts in step.
                if fresh {
                    let mut all = query("/", &self.settings());
                    all.extend(self.preamps("/"));
                    self.send(from, &all);
                }
                return;
            }
            "/unsubscribe" => {
                self.subscribers.lock_ok().retain(|s| s.to != from);
                return;
            }
            _ => {}
        }
        let settings = self.settings();
        match command_for(msg, &settings) {
            Ok(None) => {
                let mut q = query(&msg.addr, &settings);
                q.extend(self.preamps(&msg.addr));
                self.send(from, &q)
            }
            Ok(Some((cmd, _))) if cmd.is_empty() => {}
            Ok(Some((cmd, body))) => {
                // Written in the audit as this sender's (§M.25).
                let who = idn_open_protocol::web::Who::osc(from.ip());
                let done = self.console.command_as(&who, &cmd, &body);
                if let Err(e) = done {
                    // The control goes back to where the desk is.
                    let mut back = query(msg.addr.trim_end_matches("/travel"), &settings);
                    back.push(Message::new(
                        "/error",
                        vec![Arg::Str(format!("{}: {e}", msg.addr))],
                    ));
                    self.send(from, &back);
                }
            }
            Err(e) => self.send(from, &[Message::new("/error", vec![Arg::Str(e)])]),
        }
    }
}

/// The meters as OSC: each channel's level after its fader (peak dBFS), each desk
/// output's, and the main mix's loudness (momentary, short-term, integrated LUFS).
fn meter_messages(m: &Value) -> Vec<Message> {
    let floats = |key: &str, i: usize| -> Vec<Arg> {
        m[key]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|v| Arg::Float(v[i].as_f64().unwrap_or(-144.0) as f32))
                    .collect()
            })
            .unwrap_or_default()
    };
    let lufs = m["lufs"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|v| Arg::Float(v.as_f64().unwrap_or(-144.0) as f32))
                .collect()
        })
        .unwrap_or_default();
    vec![
        Message::new("/meters/ch", floats("ch", 2)),
        Message::new("/meters/out", floats("out", 0)),
        Message::new("/meters/lufs", lufs),
    ]
}

/// OSC control on a UDP port; stopped when dropped.
pub struct Osc {
    shared: Arc<Shared>,
    threads: Vec<std::thread::JoinHandle<()>>,
    port: u16,
}

impl Osc {
    /// Listens on `port` (all interfaces; 0: any free one). `allow`: the only senders
    /// listened to (empty: any on this network — loopback, private and link-local
    /// addresses; OSC has no password, so keep it to a trusted network).
    pub fn start(console: Arc<Console>, port: u16, allow: Vec<IpAddr>) -> Result<Osc, String> {
        let socket =
            UdpSocket::bind(("0.0.0.0", port)).map_err(|e| format!("OSC port {port}: {e}"))?;
        socket
            .set_read_timeout(Some(Duration::from_millis(200)))
            .map_err(|e| e.to_string())?;
        let port = socket.local_addr().map_err(|e| e.to_string())?.port();
        let shared = Arc::new(Shared {
            console,
            socket,
            allow,
            subscribers: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            cached: Mutex::new(None),
        });
        let rx = {
            let sh = shared.clone();
            std::thread::Builder::new()
                .name("idn-mixer-osc".into())
                .spawn(move || {
                    let mut buf = vec![0u8; 65536];
                    while !sh.stop.load(Ordering::Relaxed) {
                        let Ok((n, from)) = sh.socket.recv_from(&mut buf) else {
                            continue;
                        };
                        if if sh.allow.is_empty() {
                            !on_lan(from.ip())
                        } else {
                            !sh.allow.contains(&from.ip())
                        } {
                            continue;
                        }
                        match decode(&buf[..n]) {
                            Ok(msgs) => msgs.iter().for_each(|m| sh.handle(m, from)),
                            Err(e) => {
                                sh.send(from, &[Message::new("/error", vec![Arg::Str(e)])]);
                            }
                        }
                    }
                })
                .map_err(|e| e.to_string())?
        };
        // Each change, from anywhere, to the subscribers; the meters to those who asked.
        let tx = {
            let sh = shared.clone();
            std::thread::Builder::new()
                .name("idn-mixer-osc-out".into())
                .spawn(move || {
                    let mut version = 0;
                    let mut last: Option<std::collections::HashMap<String, Value>> = None;
                    let mut meters_at = Instant::now();
                    while !sh.stop.load(Ordering::Relaxed) {
                        version = sh.console.wait_change(version, Duration::from_millis(100));
                        let subs: Vec<(SocketAddr, bool)> = {
                            let mut s = sh.subscribers.lock_ok();
                            let now = Instant::now();
                            s.retain(|s| s.until > now);
                            s.iter().map(|s| (s.to, s.meters)).collect()
                        };
                        if subs.is_empty() {
                            last = None;
                            continue;
                        }
                        let now: std::collections::HashMap<String, Value> = flatten(&sh.settings())
                            .into_iter()
                            .chain(sh.console.preamp_osc())
                            .collect();
                        if let Some(before) = &last {
                            let mut changed: Vec<Message> = now
                                .iter()
                                .filter(|(a, v)| before.get(*a) != Some(v))
                                .filter_map(|(a, v)| {
                                    Some(Message::new(a.clone(), vec![arg_of(v)?]))
                                })
                                .collect();
                            changed.sort_by(|a, b| a.addr.cmp(&b.addr));
                            if !changed.is_empty() {
                                for (to, _) in &subs {
                                    sh.send(*to, &changed);
                                }
                            }
                        }
                        last = Some(now);
                        if meters_at.elapsed() >= Duration::from_millis(100) {
                            meters_at = Instant::now();
                            if subs.iter().any(|s| s.1) {
                                let m = meter_messages(&sh.console.meters());
                                for (to, _) in subs.iter().filter(|s| s.1) {
                                    sh.send(*to, &m);
                                }
                            }
                        }
                    }
                })
                .map_err(|e| e.to_string())?
        };
        Ok(Osc {
            shared,
            threads: vec![rx, tx],
            port,
        })
    }

    /// The UDP port it listens on.
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for Osc {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_become_patches() {
        let s = serde_json::to_value(crate::desk::Settings::new(8)).unwrap();
        let cmd = |addr: &str, a: Arg| {
            command_for(&Message::new(addr, vec![a]), &s)
                .unwrap()
                .unwrap()
        };
        assert_eq!(
            cmd("/ch/3/fader", Arg::Float(-6.0)),
            ("channel".into(), json!({"n": 3, "fader": -6.0}))
        );
        // An on/off setting from a float (a TouchOSC button) and from T/F.
        assert_eq!(
            cmd("/ch/1/mute", Arg::Float(1.0)).1,
            json!({"n": 1, "mute": true})
        );
        assert_eq!(
            cmd("/aux/2/mute", Arg::Bool(false)).1,
            json!({"bus": 2, "mute": false})
        );
        // Inside arrays, from 1: the others kept (null).
        assert_eq!(
            cmd("/ch/5/eq/2/gain", Arg::Int(3)).1,
            json!({"n": 5, "eq": [null, {"gain": 3.0}]})
        );
        assert_eq!(
            cmd("/ch/5/sends/2/level/travel", Arg::Float(1.0)).1,
            json!({"n": 5, "sends": [null, {"level": 10.0}]})
        );
        assert_eq!(
            cmd("/main/fader/travel", Arg::Float(0.0)).1["fader"],
            json!(-144.0)
        );
        assert_eq!(
            cmd("/mutegroup/2/on", Arg::Int(1)),
            ("mute_group".into(), json!({"group": 2, "on": true}))
        );
        assert_eq!(cmd("/fx/1/effect/size", Arg::Float(0.5)).0, "fx");
        assert_eq!(
            cmd("/desk/automix", Arg::Int(1)),
            ("desk".into(), json!({"automix": true}))
        );
        assert_eq!(
            cmd("/ch/2/name", Arg::Str("Pastor".into())).1,
            json!({"n": 2, "name": "Pastor"})
        );
        assert_eq!(
            cmd("/snapshot/recall", Arg::Str("Sunday".into())),
            ("snapshot_recall".into(), json!({"name": "Sunday"}))
        );
        assert_eq!(
            cmd("/cmd", Arg::Str("clear_solo".into())),
            ("clear_solo".into(), json!({}))
        );
        // A query, and what is refused.
        assert_eq!(
            command_for(&Message::new("/ch/3/fader", vec![]), &s).unwrap(),
            None
        );
        let err = |addr: &str| command_for(&Message::new(addr, vec![Arg::Int(1)]), &s).is_err();
        assert!(err("/ch/0/fader") && err("/nothing") && err("/ch/1/inserts") && err("/ch/1/eq"));
        assert!(err("/ch/1/mute/travel") && err("/desk/channels"));
        // The query's replies: everything under the address, faders with their travel.
        let q = query("/ch/3/fader", &s);
        assert_eq!(q.len(), 2);
        assert_eq!(q[1].addr, "/ch/3/fader/travel");
        assert!(query("/ch/3", &s).len() > 50);
    }

    #[test]
    fn a_tablet_moves_a_fader_and_hears_of_changes() {
        let cfg = crate::engine::ConsoleConfig {
            name: "OSC test".into(),
            channels: 8,
            mdns: Some(false),
            ..Default::default()
        };
        let console = Arc::new(Console::start(cfg).unwrap());
        let osc = Osc::start(console.clone(), 0, vec![]).unwrap();
        let tablet = UdpSocket::bind("127.0.0.1:0").unwrap();
        tablet
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let to = ("127.0.0.1", osc.port());
        let send = |m: Message| tablet.send_to(&encode(&m), to).unwrap();
        let recv_all = || {
            let mut got = Vec::new();
            let mut buf = vec![0u8; 65536];
            while let Ok((n, _)) = tablet.recv_from(&mut buf) {
                got.extend(decode(&buf[..n]).unwrap());
            }
            got
        };
        // Subscribing brings everything.
        send(Message::new("/subscribe", vec![]));
        let all = recv_all();
        assert!(all.iter().any(|m| m.addr == "/ch/8/fader"), "{}", all.len());
        // A fader moved by OSC changes the desk.
        send(Message::new("/ch/2/fader", vec![Arg::Float(-12.0)]));
        let back = recv_all();
        assert_eq!(console.settings().channels[1].fader, -12.0);
        assert!(
            back.iter()
                .any(|m| m.addr == "/ch/2/fader" && m.args == vec![Arg::Float(-12.0)])
        );
        // A change made elsewhere (the panel) reaches the subscriber.
        console.channel(4, &json!({"mute": true})).unwrap();
        let back = recv_all();
        assert!(
            back.iter()
                .any(|m| m.addr == "/ch/4/mute" && m.args == vec![Arg::Int(1)]),
            "{back:?}"
        );
        // A query.
        send(Message::new("/ch/2/fader", vec![]));
        let q = recv_all();
        assert!(q.iter().any(|m| m.addr == "/ch/2/fader/travel"));
        // Locked: refused, and the control sent back to where the desk is.
        console.lock(true);
        send(Message::new("/ch/2/fader", vec![Arg::Float(0.0)]));
        let back = recv_all();
        assert_eq!(console.settings().channels[1].fader, -12.0);
        assert!(back.iter().any(|m| m.addr == "/error"));
        assert!(
            back.iter()
                .any(|m| m.addr == "/ch/2/fader" && m.args == vec![Arg::Float(-12.0)])
        );
        console.lock(false);
        // Both written in the desk's audit as this sender's (§M.25).
        let log = console.audit().newest(20);
        let osc_lines: Vec<&Value> = log
            .iter()
            .filter(|l| l["user"] == "osc 127.0.0.1")
            .collect();
        assert!(osc_lines.iter().any(|l| l["ok"] == true), "{log:?}");
        assert!(
            osc_lines
                .iter()
                .any(|l| l["ok"] == false && l["error"] == "the desk is locked"),
            "{log:?}"
        );
        // Meters for those who ask.
        send(Message::new("/subscribe", vec![Arg::Str("meters".into())]));
        let mut buf = vec![0u8; 65536];
        let deadline = Instant::now() + Duration::from_secs(3);
        let ch = loop {
            assert!(Instant::now() < deadline, "no meters");
            let (n, _) = tablet.recv_from(&mut buf).unwrap();
            if let Some(m) = decode(&buf[..n])
                .unwrap()
                .into_iter()
                .find(|m| m.addr == "/meters/ch")
            {
                break m;
            }
        };
        assert_eq!(ch.args.len(), 8);
        drop(osc);
    }
}
