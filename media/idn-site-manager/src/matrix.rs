//! The routing matrix for any UI (idn-routing.md §A.6, roadmap R.32 stages 1b and 1c; the
//! UI is the client's): `GET /api/v1/matrix`, every IDN transmitter and receiver with the
//! network's devices as one table, and what a click on one of its cells means
//! (`POST /api/v1/route_cell`). What the Site Manager's own page once worked out in its
//! script, served so every UI gets the same.
//!
//! - Transmitters (`id` = `name`): IDN's (`kind` "idn"; `known` false for a Discovery
//!   Server source or a route's transmitter not listed), the network's devices' outputs
//!   (`kind` "network", `name` "DEVICE (network audio)"), and "unmatched" read-only
//!   columns (`kind` "unmatched", `unread`) for subscriptions to channels no device list
//!   has.
//! - Receivers: IDN's (`id` "idn/FINGERPRINT/NAME") and the devices' inputs (`id`
//!   "net/DEVICE", `net`, `readonly` for plain IDN routes). A machine's own IDN device is
//!   no row (its receive side is IDN's "(AoIP)" source).
//! - Routes through a machine's pool are drawn where a person expects them: an IDN
//!   receiver's route from a pool input in the device's column, a device input fed from a
//!   pool output in the IDN transmitter's column.
//! - Each route's `state` and `words` (a device subscription's status as the device gives
//!   it, [`net_status`]).

use idn_open_protocol::trust::NodeReport;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// The highest pool input a member's report is taken with (a pool is at most a few
/// hundred channels).
const MAX_POOL_INPUT: i64 = 4096;

/// What a device's columns and rows are named by after its own name.
pub(crate) const NET: &str = " (network audio)";

/// A device subscription's status (§3.7 of the device's spec): its state and the words for
/// it. Another sample rate has its own (bug run N6-3).
pub(crate) fn net_status(status: &Value) -> (&'static str, &'static str) {
    let n = match status {
        Value::Number(n) => n.as_u64().unwrap_or(0),
        Value::String(s) => {
            let s = s.trim();
            match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                Some(h) => u64::from_str_radix(h, 16).unwrap_or(0),
                None => s.parse().unwrap_or(0),
            }
        }
        _ => 0,
    };
    match n & 0xffff {
        0 => ("", ""),
        9 | 10 => ("ok", "working"),
        8 => ("connecting", "being set up"),
        1 => ("no_source", "source not found"),
        0x10 | 0x11 => ("rates_differ", "sample rates don\u{2019}t match"),
        0x14 | 0x15 => ("failed", "failed"),
        _ => ("no_channel", "no such channel"),
    }
}

/// The words for a route's state (IDN's own states and the devices').
pub(crate) fn words(state: &str) -> &'static str {
    match state {
        "ok" => "working",
        "connecting" => "being set up",
        "no_source" => "source not found",
        "no_channel" => "no such channel",
        "rates_differ" => "sample rates don\u{2019}t match",
        "failed" => "failed",
        _ => "",
    }
}

/// A text field (empty when missing).
fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

/// A number given as a number or as text.
fn num(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn arr(v: &Value) -> &[Value] {
    v.as_array().map_or(&[], Vec::as_slice)
}

/// "maker model" of a device.
fn what(d: &Value) -> String {
    [s(d, "maker"), s(d, "model")]
        .into_iter()
        .filter(|x| !x.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// A channel's label: its name, else its number.
fn label(c: &Value) -> String {
    match s(c, "name") {
        "" => c
            .get("n")
            .and_then(num)
            .map_or(String::new(), |n| n.to_string()),
        n => n.to_owned(),
    }
}

/// The matrix of the site now: the machines' `reports` and the Discovery Server's other
/// `sources`.
pub(crate) fn build(reports: &[NodeReport], sources: &[String]) -> Value {
    from_parts(
        crate::status::audio_json(reports, sources),
        &crate::netaudio::devices_json(reports),
    )
}

/// One machine's pool, as the matrix draws it.
struct Pool {
    source: String,
    own: String,
    ins: Vec<Value>,
    outs: Vec<(Value, String)>,
}

/// The matrix from `/api/v1/status`'s `audio` and `/api/v1/network-devices`.
fn from_parts(audio: Value, net: &Value) -> Value {
    let machines = arr(&net["machines"]);
    let read_only_on: Vec<Value> = arr(&net["read_only"]).to_vec();
    let read_only = !read_only_on.is_empty();
    // Each device once, though every machine running a device reads the whole network; the
    // reading that names it "this" (its own) wins, so an IDN device stays out of the rows
    // whoever read it (bug run I4-6).
    let mut by_name: BTreeMap<String, Value> = BTreeMap::new();
    let mut read_by: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut ours: BTreeSet<String> = BTreeSet::new();
    for m in machines {
        for d in arr(&m["devices"]) {
            let name = s(d, "name");
            if name.is_empty() {
                continue;
            }
            let by = match s(m, "name") {
                "" => s(m, "fingerprint"),
                n => n,
            };
            read_by.entry(name.into()).or_default().push(by.into());
            if s(d, "this") == "true" {
                ours.insert(name.into());
                by_name.insert(name.into(), d.clone());
            } else {
                by_name.entry(name.into()).or_insert_with(|| d.clone());
            }
        }
    }
    let devs: Vec<Value> = by_name
        .into_values()
        .map(|mut d| {
            if ours.contains(s(&d, "name")) {
                d["this"] = "true".into();
            }
            d
        })
        .collect();
    let mut txs: Vec<Value> = arr(&audio["transmitters"]).to_vec();
    for t in &mut txs {
        t["id"] = t["name"].clone();
        t["kind"] = "idn".into();
    }
    let mut rxs: Vec<Value> = arr(&audio["receivers"]).to_vec();
    for r in &mut rxs {
        r["id"] = format!("idn/{}/{}", s(r, "fingerprint"), s(r, "name")).into();
        r["kind"] = "idn".into();
    }
    // Each device's outputs' labels.
    let mut tx_names: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for d in &devs {
        let labels: Vec<String> = arr(&d["tx"]).iter().map(label).collect();
        tx_names.insert(s(d, "name").into(), labels.clone());
        if !labels.is_empty() {
            // `device` asks by the mDNS instance; a subscription names the device by its
            // name (bug run K5-7).
            let name = format!("{}{NET}", s(d, "name"));
            txs.push(json!({
                "id": name, "name": name, "kind": "network", "channels": labels.len(),
                "labels": labels, "machine": what(d), "known": true, "netaudio": true,
                "device": device_of(d), "devname": s(d, "name"), "own": s(d, "this") == "true",
            }));
        }
    }
    // Subscriptions to a channel no read list has (a device not read, or a name its list
    // doesn't have: renamed, or by its other name): a read-only column of their own, by
    // those names (bug runs K5-1, K5-2).
    let mut unmatched: Vec<Value> = Vec::new();
    let mut unmatched_col = |dev: &str, ch: &str| -> (String, usize) {
        let read = tx_names.contains_key(dev);
        let name = format!("{dev}{NET}{}", if read { " ?" } else { "" });
        let i = match unmatched.iter().position(|u| u["name"] == name.as_str()) {
            Some(i) => i,
            None => {
                unmatched.push(json!({
                    "id": name, "name": name, "kind": "unmatched", "channels": 0, "labels": [],
                    "machine": if read { "channels not in its list" } else { "not read" },
                    "known": true, "netaudio": true, "unread": true,
                }));
                unmatched.len() - 1
            }
        };
        let u = &mut unmatched[i];
        let labels = u["labels"].as_array_mut().expect("labels");
        let at = match labels.iter().position(|l| l == ch) {
            Some(at) => at,
            None => {
                labels.push(ch.into());
                labels.len() - 1
            }
        };
        u["channels"] = (u["labels"].as_array().map_or(0, Vec::len)).into();
        (name, at)
    };
    // The pools (R.32 stage 1c, K.12): each machine's network audio device, its IDN source
    // and what its channels carry.
    let mut pools: Vec<Pool> = Vec::new();
    for m in machines {
        let p = &m["pool"];
        if !p.is_object() {
            continue;
        }
        let Some(own) = arr(&m["devices"]).iter().find(|d| s(d, "this") == "true") else {
            continue;
        };
        // A pool input is drawn as its device's only while the device's own list still has
        // it subscribed there (bug run L2-4).
        let ins: Vec<Value> = arr(&p["ins"])
            .iter()
            .filter(|i| {
                arr(&own["rx"]).iter().any(|c| {
                    num(&c["n"]).is_some()
                        && num(&c["n"]) == num(&i["k"])
                        && s(c, "from_device") == s(i, "device")
                        && s(c, "from_channel") == s(i, "channel")
                })
            })
            .cloned()
            .collect();
        // A pool output, only while its IDN route (the device's receiver "AoIP") is still
        // the pool's.
        let aoip = rxs
            .iter()
            .find(|x| s(x, "fingerprint") == s(m, "fingerprint") && s(x, "name") == "AoIP");
        let outs: Vec<(Value, String)> = arr(&p["outs"])
            .iter()
            .filter(|o| {
                aoip.is_some_and(|a| {
                    arr(&a["routes"]).iter().any(|r| {
                        num(&r["ch"]).is_some_and(|c| Some(c + 1) == num(&o["k"]))
                            && s(r, "tx") == s(o, "tx")
                            && num(&r["tx_ch"]).is_some()
                            && num(&r["tx_ch"]) == num(&o["tx_ch"])
                    })
                })
            })
            .map(|o| {
                let k = num(&o["k"]);
                let name = arr(&own["tx"])
                    .iter()
                    .find(|c| num(&c["n"]).is_some() && num(&c["n"]) == k)
                    .map(|c| s(c, "name"))
                    .filter(|n| !n.is_empty())
                    .map_or_else(|| k.unwrap_or(0).to_string(), str::to_owned);
                (o.clone(), name)
            })
            .collect();
        pools.push(Pool {
            source: s(p, "source").into(),
            own: s(own, "name").into(),
            ins,
            outs,
        });
    }
    // An IDN receiver's route from a pool input: drawn in the device's own column (where
    // that column can't take it, left in the source's, where it can be cleared; L2-8).
    for rx in &mut rxs {
        let Some(routes) = rx["routes"].as_array_mut() else {
            continue;
        };
        for r in routes {
            let Some(p) = pools.iter().find(|p| p.source == s(r, "tx")) else {
                continue;
            };
            let Some(i) = p.ins.iter().find(|i| {
                num(&i["k"]).is_some() && num(&i["k"]) == num(&r["tx_ch"]).map(|c| c + 1)
            }) else {
                continue;
            };
            let at = tx_names
                .get(s(i, "device"))
                .and_then(|l| l.iter().position(|x| x == s(i, "channel")));
            if let Some(at) = at {
                r["tx"] = format!("{}{NET}", s(i, "device")).into();
                r["tx_ch"] = at.into();
                r["through"] = p.source.clone().into();
            }
        }
    }
    // The devices' inputs as rows (not a machine's own IDN device: its receive side is IDN's
    // "(AoIP)" source already).
    for d in &devs {
        if s(d, "this") == "true" || arr(&d["rx"]).is_empty() {
            continue;
        }
        let mut routes = Vec::new();
        for (ch, c) in arr(&d["rx"]).iter().enumerate() {
            let from = s(c, "from_device");
            if from.is_empty() {
                continue;
            }
            let state = match net_status(&c["status"]).0 {
                "" => "no_channel",
                st => st,
            };
            // From a pool output: drawn in the IDN transmitter's column.
            let out = pools
                .iter()
                .find(|p| p.own == from)
                .and_then(|p| p.outs.iter().find(|(_, n)| n == s(c, "from_channel")));
            let (tx, tx_ch) = if let Some((o, _)) = out {
                (
                    s(o, "tx").to_owned(),
                    num(&o["tx_ch"]).unwrap_or(0) as usize,
                )
            } else if let Some(i) = tx_names
                .get(from)
                .and_then(|l| l.iter().position(|x| x == s(c, "from_channel")))
            {
                (format!("{from}{NET}"), i)
            } else {
                let ch_name = match s(c, "from_channel") {
                    "" => "?",
                    x => x,
                };
                unmatched_col(from, ch_name)
            };
            routes.push(json!({"ch": ch, "tx": tx, "tx_ch": tx_ch, "state": state}));
        }
        let name = s(d, "name");
        rxs.push(json!({
            "id": format!("net/{name}"), "kind": "network", "name": format!("{name}{NET}"),
            "machine": what(d), "channels": arr(&d["rx"]).len(),
            "labels": arr(&d["rx"]).iter().map(label).collect::<Vec<_>>(),
            "routes": routes, "readonly": true, "app": "network", "net": true,
            "device": device_of(d), "devname": name,
        }));
    }
    txs.extend(unmatched);
    // Routes to a transmitter not listed (gone, or named by address) get a column too.
    for rx in &rxs {
        for r in arr(&rx["routes"]) {
            let tx = s(r, "tx");
            if tx.is_empty() {
                continue;
            }
            let want = num(&r["tx_ch"]).map_or(2, |c| (c + 1).max(2));
            match txs.iter_mut().find(|t| s(t, "name") == tx) {
                Some(t) if t["known"] != true => {
                    let have = t["channels"].as_i64().unwrap_or(0);
                    t["channels"] = have.max(want).into();
                }
                Some(_) => {}
                None => txs.push(json!({
                    "id": tx, "name": tx, "kind": "idn", "channels": want, "labels": [],
                    "machine": "", "known": false,
                })),
            }
        }
    }
    for rx in &mut rxs {
        for r in rx["routes"].as_array_mut().into_iter().flatten() {
            let w = words(s(r, "state"));
            r["words"] = w.into();
        }
    }
    // A pool's IDN source carries network devices' channels (§A.16): its origin, channel
    // by channel ("DEVICE/CHANNEL"; pool input k is its channel k − 1).
    for p in &pools {
        let Some(t) = txs.iter_mut().find(|t| s(t, "name") == p.source) else {
            continue;
        };
        if !t["origin"].is_null() || p.ins.is_empty() {
            continue;
        }
        // (A member's word: a pool input past any pool's size is left out, not a list
        // of billions made.)
        let k_of = |i: &Value| num(&i["k"]).filter(|k| (1..=MAX_POOL_INPUT).contains(k));
        let n = p.ins.iter().filter_map(k_of).max().unwrap_or(0) as usize;
        let mut chans = vec![String::new(); n];
        for i in &p.ins {
            if let Some(k) = k_of(i) {
                chans[k as usize - 1] = format!("{}/{}", s(i, "device"), s(i, "channel"));
            }
        }
        t["origin"] = json!({"kind": "network_device", "device": "", "channels": chans});
    }
    // Who receives each transmitter (§A.16): per receiver, its channels from it and how.
    received_by(&mut txs, &rxs);
    let devices: Vec<Value> = devs
        .iter()
        .map(|d| {
            let name = s(d, "name");
            let own = s(d, "this") == "true";
            let renamable = !own && !read_only;
            json!({
                "name": name, "instance": s(d, "instance"), "maker": s(d, "maker"),
                "model": s(d, "model"), "own": own, "read_only": read_only,
                "renamable": renamable,
                // Channels only announced (the device refused reads) are named as announced.
                "outputs_renamable": renamable && s(d, "tx_from") != "announced",
                "inputs": arr(&d["rx"]).len(), "outputs": arr(&d["tx"]).len(),
                "read_by": read_by.get(name).cloned().unwrap_or_default(),
            })
        })
        .collect();
    json!({
        "read_only": read_only,
        "read_only_on": read_only_on,
        "transmitters": txs,
        "receivers": rxs,
        "devices": devices,
    })
}

/// Each transmitter's `received_by` (idn-routing.md §A.16): one entry per receiver with a
/// route from it — `machine`, `fingerprint`, `program` (the app), `receiver`, `kind` (the
/// receiver's: program, sound_card, …, network_device for a device's inputs), `via`
/// (machine, network, or mixed across its channels), and `channels` (`{"ch", "tx_ch",
/// "state", "via"}`).
fn received_by(txs: &mut [Value], rxs: &[Value]) {
    let mut by: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for rx in rxs {
        let net = rx["net"] == true;
        let mut per_tx: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for r in arr(&rx["routes"]) {
            let tx = s(r, "tx");
            if tx.is_empty() {
                continue;
            }
            per_tx.entry(tx.to_owned()).or_default().push(json!({
                "ch": r["ch"], "tx_ch": r["tx_ch"], "state": r["state"],
                "via": if net { json!("network") } else { r["via"].clone() },
                "through": r.get("through").cloned().unwrap_or(Value::Null),
            }));
        }
        for (tx, channels) in per_tx {
            let vias: BTreeSet<&str> = channels.iter().filter_map(|c| c["via"].as_str()).collect();
            let via = match vias.len() {
                0 => Value::Null,
                1 => json!(vias.iter().next()),
                _ => json!("mixed"),
            };
            by.entry(tx).or_default().push(json!({
                "machine": rx["machine"],
                "fingerprint": rx.get("fingerprint").cloned().unwrap_or(Value::Null),
                "program": rx["app"],
                "receiver": rx["name"],
                "receiver_id": rx["id"],
                "kind": if net { "network_device" } else { rx["rx_kind"].as_str().unwrap_or("other") },
                "via": via,
                "channels": channels,
            }));
        }
    }
    for t in txs.iter_mut() {
        t["received_by"] = Value::Array(by.remove(s(t, "name")).unwrap_or_default());
    }
}

/// `GET /api/v1/audio_receivers`: each transmitter with its origin and who receives it,
/// from the matrix `m` (`{"transmitters": [{"id", "name", "machine", "kind", "origin",
/// "received_by"}]}`).
pub(crate) fn receivers_of_outputs(m: &Value) -> Value {
    let txs: Vec<Value> = arr(&m["transmitters"])
        .iter()
        .map(|t| {
            json!({
                "id": t["id"], "name": t["name"], "machine": t["machine"], "kind": t["kind"],
                "channels": t["channels"], "labels": t["labels"],
                "origin": t.get("origin").cloned().unwrap_or(Value::Null),
                "received_by": t["received_by"],
            })
        })
        .collect();
    json!({ "transmitters": txs })
}

/// What a request names a device by: its mDNS instance (unique), else its name.
fn device_of(d: &Value) -> &str {
    match s(d, "instance") {
        "" => s(d, "name"),
        i => i,
    }
}

/// What a click on a cell does: a request to the API's own change, or nothing (already
/// so).
#[derive(Debug, PartialEq)]
pub(crate) enum CellAction {
    Post(&'static str, Value),
    Nothing(&'static str),
}

/// `POST /api/v1/route_cell {receiver_id, ch, tx_id, tx_ch, on}` on the matrix `m`
/// ([`build`]): the receiver's channel `ch` (from 0) from the transmitter's channel `tx_ch`
/// (from 0), `on` the state wanted (none: the other of now, as a click). As the page did:
/// between two devices a subscription; between a device and IDN through a pool (cleared:
/// the IDN receiver's route, or the device input's subscription); IDN to IDN a route.
pub(crate) fn cell_action(m: &Value, v: &Value) -> Result<CellAction, String> {
    let ch = v
        .get("ch")
        .and_then(num)
        .filter(|c| (0..256).contains(c))
        .ok_or("ch: the receiver's channel, 0 to 255")?;
    let tx_ch = v
        .get("tx_ch")
        .and_then(num)
        .filter(|c| (0..256).contains(c))
        .ok_or("tx_ch: the transmitter's channel, 0 to 255")?;
    let rid = s(v, "receiver_id");
    let tid = s(v, "tx_id");
    let rx = arr(&m["receivers"])
        .iter()
        .find(|r| s(r, "id") == rid)
        .ok_or_else(|| format!("no receiver {rid:?} in the matrix"))?;
    let tx = arr(&m["transmitters"])
        .iter()
        .find(|t| s(t, "id") == tid)
        .ok_or_else(|| format!("no transmitter {tid:?} in the matrix"))?;
    let flag = |x: &Value, k: &str| x[k] == true;
    let (rx_net, tx_net) = (flag(rx, "net"), flag(tx, "netaudio"));
    let rx_channels = rx["channels"].as_i64().unwrap_or(0);
    let tx_channels = tx["channels"].as_i64().unwrap_or(0);
    if rx_net && ch >= rx_channels {
        return Err(format!("{} has {rx_channels} inputs", s(rx, "devname")));
    }
    if tx_net && tx_ch >= tx_channels {
        return Err(format!("{} has {tx_channels} channels", s(tx, "name")));
    }
    let now = arr(&rx["routes"])
        .iter()
        .find(|r| num(&r["ch"]) == Some(ch))
        .is_some_and(|r| s(r, "tx") == s(tx, "name") && num(&r["tx_ch"]) == Some(tx_ch));
    let want = v.get("on").and_then(Value::as_bool).unwrap_or(!now);
    if want == now {
        return Ok(CellAction::Nothing(if now {
            "already routed there"
        } else {
            "not routed there"
        }));
    }
    let unread = flag(tx, "unread");
    let net_cell = rx_net && tx_net && !unread;
    let pool_cell = !unread
        && !flag(tx, "own")
        && ((rx_net && !tx_net && flag(tx, "known"))
            || (!rx_net && !flag(rx, "readonly") && tx_net));
    if (net_cell || pool_cell) && m["read_only"] == true {
        return Err(
            "the network's devices are read only on this site (a machine's network audio setting)"
                .into(),
        );
    }
    let device = s(rx, "device");
    let unsubscribe = || json!({"op": "unsubscribe", "device": device, "channel": ch + 1});
    // The device's channel as a subscription names it: its label, else its number.
    let tx_channel = || {
        tx["labels"]
            .get(tx_ch as usize)
            .and_then(Value::as_str)
            .filter(|l| !l.is_empty())
            .map_or_else(|| (tx_ch + 1).to_string(), str::to_owned)
    };
    let route = |to: Option<(&str, i64)>| {
        let change = match to {
            Some((tx, tx_ch)) => json!({"ch": ch, "tx": tx, "tx_ch": tx_ch}),
            None => json!({"ch": ch, "tx": ""}),
        };
        json!({"fingerprint": s(rx, "fingerprint"), "receiver": s(rx, "name"), "changes": [change]})
    };
    Ok(if net_cell {
        // Between two devices (R.32 stage 1b): the receiving device's subscription.
        let body = if want {
            let txd = match s(tx, "devname") {
                "" => s(tx, "device"),
                n => n,
            };
            json!({"op": "subscribe", "device": device, "channel": ch + 1,
                "tx_device": txd, "tx_channel": tx_channel()})
        } else {
            unsubscribe()
        };
        CellAction::Post("/api/v1/network-devices/do", body)
    } else if pool_cell && rx_net {
        // An IDN transmitter to a device's input, through a pool output.
        if want {
            CellAction::Post(
                "/api/v1/network-devices/pool",
                json!({"dir": "out", "device": device, "channel": ch + 1,
                    "tx": s(tx, "name"), "tx_ch": tx_ch}),
            )
        } else {
            CellAction::Post("/api/v1/network-devices/do", unsubscribe())
        }
    } else if pool_cell {
        // A device's output to an IDN receiver, through a pool input.
        if want {
            CellAction::Post(
                "/api/v1/network-devices/pool",
                json!({"dir": "in", "device": s(tx, "device"), "tx_channel": tx_channel(),
                    "fingerprint": s(rx, "fingerprint"), "receiver": s(rx, "name"), "ch": ch}),
            )
        } else {
            CellAction::Post("/api/v1/route", route(None))
        }
    } else if unread {
        return Err("a subscription to a channel no device's list has: read only here (clear it on the device: network-devices/do unsubscribe)".into());
    } else if flag(tx, "own") && !rx_net {
        return Err(format!(
            "{} is a machine's own IDN device: route from its IDN transmitter",
            s(tx, "devname")
        ));
    } else if flag(rx, "readonly") || tx_net {
        return Err("this cell can't be routed".into());
    } else {
        CellAction::Post("/api/v1/route", route(want.then(|| (s(tx, "name"), tx_ch))))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn report(fp: &str, xml: &str) -> NodeReport {
        NodeReport {
            fingerprint: fp.into(),
            report: idn_open_protocol::xml::parse(xml).unwrap(),
            age: Duration::ZERO,
            address: None,
        }
    }

    /// A site: machine A (IDN only: a desk receiver), machine B running a network audio
    /// device (B-IDN) with its pool, both reading the stage box "Box"; "Amp" subscribed to
    /// Box, to a pool output and to a device no one reads.
    fn site() -> Vec<NodeReport> {
        vec![
            report(
                "aa",
                r#"<idn_node_report name="A"><audio_tx name="A (Mic)" channels="2"/>
                <audio_rx name="Desk" channels="4"><route ch="0" tx="A (Mic)" tx_ch="1" state="ok"/><route ch="1" tx="B-IDN (AoIP)" tx_ch="2" state="connecting"/><route ch="2" tx="Gone (X)" tx_ch="5"/></audio_rx>
                <network_devices><device name="Box" instance="Box-1" maker="Acme" model="S16"><tx n="1" name="Vox"/><tx n="2" name="Gtr"/></device><device name="B-IDN" instance="B-IDN"><rx n="1" name="01"/></device></network_devices></idn_node_report>"#,
            ),
            report(
                "bb",
                r#"<idn_node_report name="B"><audio_tx name="B-IDN (AoIP)" channels="8"/>
                <audio_rx name="AoIP" channels="8"><route ch="0" tx="A (Mic)" tx_ch="0" state="ok"/></audio_rx>
                <network_devices><device name="B-IDN" instance="B-IDN" this="true"><rx n="3" name="03" from_device="Box" from_channel="Gtr" status="9"/><tx n="1" name="p1"/></device><device name="Box" instance="Box-1"><tx n="1" name="Vox"/><tx n="2" name="Gtr"/></device>
                <device name="Amp" instance="Amp" tx_from="announced"><rx n="1" name="L" from_device="Box" from_channel="Vox" status="0x10"/><rx n="2" name="R" from_device="B-IDN" from_channel="p1" status="9"/><rx n="3" name="C" from_device="Far" from_channel="x" status="1"/><rx n="4" name="D"/></device>
                <pool source="B-IDN (AoIP)" in="8" out="8"><in k="3" device="Box" channel="Gtr"/><out k="1" tx="A (Mic)" tx_ch="0"/></pool></network_devices></idn_node_report>"#,
            ),
        ]
    }

    fn find<'a>(list: &'a Value, id: &str) -> &'a Value {
        arr(list)
            .iter()
            .find(|x| s(x, "id") == id)
            .unwrap_or_else(|| panic!("no {id}"))
    }

    /// Who receives each transmitter (§A.16): per receiver, its kind, its channels and how
    /// they arrive (this machine's ring, the network, multicast); a transmitter's origin as
    /// its program said, and a pool source's from its inputs.
    #[test]
    fn who_receives_each_output() {
        let reports = vec![
            report(
                "aa",
                r#"<idn_node_report name="A"><audio_tx name="A (Mic)" channels="2" origin_kind="sound_card" origin_device="Focusrite" origin_channels="In 1,In 2"/>
                <audio_rx name="Desk" channels="4" app="idn-mixer" kind="desk"><route ch="0" tx="A (Mic)" tx_ch="0" state="ok" via="machine"/><route ch="1" tx="A (Mic)" tx_ch="1" state="ok" via="machine"/></audio_rx>
                <audio_rx name="Old" channels="2" app="old-app"><route ch="0" tx="A (Mic)" tx_ch="0" state="connecting"/></audio_rx></idn_node_report>"#,
            ),
            report(
                "bb",
                r#"<idn_node_report name="B"><audio_tx name="B-IDN (AoIP)" channels="8"/>
                <audio_rx name="Stage" channels="2" app="idn-audio" kind="sound_card"><route ch="0" tx="A (Mic)" tx_ch="1" state="ok" via="multicast"/><route ch="1" tx="A (Mic)" tx_ch="0" state="ok" via="quic"/></audio_rx>
                <network_devices><device name="B-IDN" instance="B-IDN" this="true"><rx n="2" name="02" from_device="Box" from_channel="Gtr" status="9"/></device><device name="Box" instance="Box-1"><tx n="1" name="Vox"/><tx n="2" name="Gtr"/></device>
                <pool source="B-IDN (AoIP)" in="8" out="8"><in k="2" device="Box" channel="Gtr"/></pool></network_devices></idn_node_report>"#,
            ),
        ];
        let m = build(&reports, &[]);
        let mic = find(&m["transmitters"], "A (Mic)");
        assert_eq!(mic["origin"]["device"], "Focusrite");
        assert_eq!(mic["origin"]["channels"][1], "In 2");
        let by = arr(&mic["received_by"]);
        assert_eq!(by.len(), 3, "{by:?}");
        let of = |name: &str| by.iter().find(|r| r["receiver"] == name).unwrap();
        let desk = of("Desk");
        assert_eq!(
            (
                desk["kind"].as_str(),
                desk["via"].as_str(),
                desk["program"].as_str()
            ),
            (Some("desk"), Some("machine"), Some("idn-mixer"))
        );
        assert_eq!(arr(&desk["channels"]).len(), 2);
        assert_eq!(of("Old")["kind"], "other");
        assert!(of("Old")["via"].is_null(), "no sound yet: no via");
        let stage = of("Stage");
        assert_eq!(stage["via"], "network");
        assert_eq!(stage["machine"], "B");
        let route = &find(&m["receivers"], "idn/bb/Stage")["routes"][0];
        assert_eq!(
            (
                route["via"].as_str(),
                route["cast"].as_str(),
                route["transport"].as_str()
            ),
            (Some("network"), Some("multicast"), Some("multicast"))
        );
        assert_eq!(find(&m["receivers"], "idn/aa/Desk")["rx_kind"], "desk");
        // The pool's source: its channel 1 (pool input 2) is Box's Gtr.
        let pool = find(&m["transmitters"], "B-IDN (AoIP)");
        assert_eq!(pool["origin"]["kind"], "network_device");
        assert_eq!(pool["origin"]["channels"][1], "Box/Gtr");
        // The same, by itself.
        let r = receivers_of_outputs(&m);
        assert_eq!(
            find(&r["transmitters"], "A (Mic)")["received_by"],
            mic["received_by"]
        );
    }

    #[test]
    fn the_matrix_with_the_networks_devices_and_pools() {
        let m = build(&site(), &["C (Talk)".into(), "A (Mic)".into()]);
        let (txs, rxs) = (&m["transmitters"], &m["receivers"]);
        assert_eq!(m["read_only"], false);
        // IDN's transmitters, a Discovery Server guess, the devices' outputs (one Box though
        // two machines read it), the unmatched column, a route's unlisted transmitter.
        assert_eq!(find(txs, "A (Mic)")["kind"], "idn");
        assert_eq!(find(txs, "C (Talk)")["known"], false);
        let bx = find(txs, "Box (network audio)");
        assert_eq!(
            (bx["device"].as_str(), bx["channels"].as_u64()),
            (Some("Box-1"), Some(2))
        );
        assert_eq!(
            arr(txs).iter().filter(|t| s(t, "devname") == "Box").count(),
            1
        );
        assert_eq!(find(txs, "B-IDN (network audio)")["own"], true);
        let far = find(txs, "Far (network audio)");
        assert_eq!(
            (far["kind"].as_str(), far["unread"].as_bool()),
            (Some("unmatched"), Some(true))
        );
        assert_eq!(find(txs, "Gone (X)")["channels"], 6);
        // The pool input drawn in Box's column; the plain route stays.
        let desk = find(rxs, "idn/aa/Desk");
        let r1 = &desk["routes"][1];
        assert_eq!(
            (s(r1, "tx"), r1["tx_ch"].as_u64(), s(r1, "words")),
            ("Box (network audio)", Some(1), "being set up")
        );
        assert_eq!(desk["routes"][0]["words"], "working");
        // The devices' inputs: not B-IDN's own (it is IDN's "(AoIP)"); Amp's routes: Box
        // (rates differ), the pool output in A (Mic)'s column, the unmatched one.
        assert!(arr(rxs).iter().all(|r| s(r, "id") != "net/B-IDN"));
        let amp = find(rxs, "net/Amp");
        assert_eq!(amp["channels"], 4);
        let rs = arr(&amp["routes"]);
        assert_eq!(rs.len(), 3);
        assert_eq!(
            (s(&rs[0], "tx"), s(&rs[0], "state"), s(&rs[0], "words")),
            (
                "Box (network audio)",
                "rates_differ",
                "sample rates don\u{2019}t match"
            )
        );
        assert_eq!(
            (s(&rs[1], "tx"), rs[1]["tx_ch"].as_u64()),
            ("A (Mic)", Some(0))
        );
        assert_eq!(
            (s(&rs[2], "tx"), s(&rs[2], "state")),
            ("Far (network audio)", "no_source")
        );
        // The devices' flags.
        let devs = &m["devices"];
        let dev = |n: &str| {
            arr(devs)
                .iter()
                .find(|d| s(d, "name") == n)
                .unwrap()
                .clone()
        };
        assert_eq!(
            (
                dev("B-IDN")["own"].as_bool(),
                dev("B-IDN")["renamable"].as_bool()
            ),
            (Some(true), Some(false))
        );
        assert_eq!(
            (
                dev("Amp")["renamable"].as_bool(),
                dev("Amp")["outputs_renamable"].as_bool()
            ),
            (Some(true), Some(false))
        );
        assert_eq!(dev("Box")["read_by"].as_array().unwrap().len(), 2);
        // A machine set read only: the site is.
        let mut ro = site();
        ro[0].report = idn_open_protocol::xml::parse(
            r#"<idn_node_report name="A"><netaudio read_only="true"/></idn_node_report>"#,
        )
        .unwrap();
        let m = build(&ro, &[]);
        assert_eq!(
            (m["read_only"].as_bool(), m["read_only_on"][0].as_str()),
            (Some(true), Some("A"))
        );
        assert!(arr(&m["devices"]).iter().all(|d| d["renamable"] == false));
    }

    #[test]
    fn what_a_click_on_each_cell_means() {
        let m = build(&site(), &[]);
        let click = |rx: &str, ch: i64, tx: &str, tx_ch: i64, on: Option<bool>| {
            let mut v = json!({"receiver_id": rx, "ch": ch, "tx_id": tx, "tx_ch": tx_ch});
            if let Some(on) = on {
                v["on"] = on.into();
            }
            cell_action(&m, &v)
        };
        let post = |p: &'static str, b: Value| Ok(CellAction::Post(p, b));
        // IDN to IDN: a route; clicked again (it is on), cleared; already so, nothing.
        assert_eq!(
            click("idn/aa/Desk", 3, "A (Mic)", 0, None),
            post(
                "/api/v1/route",
                json!({"fingerprint": "aa", "receiver": "Desk", "changes": [{"ch": 3, "tx": "A (Mic)", "tx_ch": 0}]})
            )
        );
        assert_eq!(
            click("idn/aa/Desk", 0, "A (Mic)", 1, None),
            post(
                "/api/v1/route",
                json!({"fingerprint": "aa", "receiver": "Desk", "changes": [{"ch": 0, "tx": ""}]})
            )
        );
        assert_eq!(
            click("idn/aa/Desk", 0, "A (Mic)", 1, Some(true)),
            Ok(CellAction::Nothing("already routed there"))
        );
        // A device's output to an IDN receiver: through the pool; the pool route cleared.
        assert_eq!(
            click("idn/aa/Desk", 3, "Box (network audio)", 0, None),
            post(
                "/api/v1/network-devices/pool",
                json!({"dir": "in", "device": "Box-1", "tx_channel": "Vox", "fingerprint": "aa", "receiver": "Desk", "ch": 3})
            )
        );
        assert_eq!(
            click("idn/aa/Desk", 1, "Box (network audio)", 1, None),
            post(
                "/api/v1/route",
                json!({"fingerprint": "aa", "receiver": "Desk", "changes": [{"ch": 1, "tx": ""}]})
            )
        );
        // Device to device: a subscription by names; cleared.
        assert_eq!(
            click("net/Amp", 3, "Box (network audio)", 1, Some(true)),
            post(
                "/api/v1/network-devices/do",
                json!({"op": "subscribe", "device": "Amp", "channel": 4, "tx_device": "Box", "tx_channel": "Gtr"})
            )
        );
        assert_eq!(
            click("net/Amp", 0, "Box (network audio)", 0, None),
            post(
                "/api/v1/network-devices/do",
                json!({"op": "unsubscribe", "device": "Amp", "channel": 1})
            )
        );
        // An IDN transmitter to a device's input: a pool output; the one there cleared.
        assert_eq!(
            click("net/Amp", 3, "A (Mic)", 1, None),
            post(
                "/api/v1/network-devices/pool",
                json!({"dir": "out", "device": "Amp", "channel": 4, "tx": "A (Mic)", "tx_ch": 1})
            )
        );
        assert_eq!(
            click("net/Amp", 1, "A (Mic)", 0, None),
            post(
                "/api/v1/network-devices/do",
                json!({"op": "unsubscribe", "device": "Amp", "channel": 2})
            )
        );
        // Read only: the unmatched column, IDN's own device's outputs to IDN, no such
        // receiver or channel.
        assert!(click("net/Amp", 2, "Far (network audio)", 0, None).is_err());
        assert!(
            click("idn/aa/Desk", 3, "B-IDN (network audio)", 0, None)
                .unwrap_err()
                .contains("own IDN device")
        );
        assert!(click("idn/zz/Desk", 0, "A (Mic)", 0, None).is_err());
        assert!(click("net/Amp", 9, "A (Mic)", 0, None).is_err());
        assert!(click("idn/aa/Desk", 0, "A (Mic)", 300, None).is_err());
        // A read-only site: no network changes, IDN routes as ever.
        let mut ro = m.clone();
        ro["read_only"] = true.into();
        assert!(
            cell_action(
                &ro,
                &json!({"receiver_id": "net/Amp", "ch": 3, "tx_id": "A (Mic)", "tx_ch": 0})
            )
            .unwrap_err()
            .contains("read only")
        );
        assert!(
            cell_action(
                &ro,
                &json!({"receiver_id": "idn/aa/Desk", "ch": 3, "tx_id": "A (Mic)", "tx_ch": 0})
            )
            .is_ok()
        );
    }

    #[test]
    fn a_devices_status_in_words() {
        for (st, want) in [
            (json!("9"), "ok"),
            (json!(0x1000a), "ok"),
            (json!("0x10"), "rates_differ"),
            (json!(0x15), "failed"),
            (json!(8), "connecting"),
            (json!(1), "no_source"),
            (json!(0x20), "no_channel"),
            (json!(0), ""),
            (Value::Null, ""),
        ] {
            assert_eq!(net_status(&st).0, want, "{st}");
        }
        assert_eq!(words("no_source"), "source not found");
    }
}
