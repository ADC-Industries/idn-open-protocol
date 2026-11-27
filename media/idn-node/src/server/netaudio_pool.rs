//! The pool (roadmap R.32 stage 1c, idn-aoip-controller.md K.3): routes between the
//! network's devices and IDN in one click, through this machine's network-audio device.
//!
//! - **Device → IDN** (`pool_in`): one of the device's free receive channels `k` is
//!   subscribed to the other device's output (a request to it, as any controller's); the
//!   Site Manager then routes the IDN receiver from the device's IDN source, channel `k`.
//!   A device output already on one of its channels is used again.
//! - **IDN → device** (`pool_out`): one of its free transmit channels `k` is fed from the
//!   IDN transmitter (a route of its IDN receiver, here) and the other device's input is
//!   subscribed to it.
//! - A pool channel is let go once nothing uses it (`pool_sweep`, from the Site Manager,
//!   which sees every machine's routes), not sooner than a minute after it was taken.
//!
//! Only channels nobody uses are taken, and only what the pool made is ever undone: a
//! subscription or route someone made by hand (Dante Controller, the matrix) is left alone
//! (K.3). The table is kept in `netaudio-pool.xml` in the Node's folder.

use super::netaudio_do::{
    ask, find_device, next_id, subscribe_request, target_of, unsubscribe_request,
};
use super::*;
use idn_aoip_device::control;
use idn_open_protocol::node::routing::RouteChange;
use idn_open_protocol::xml::Element;

const FILE: &str = "netaudio-pool.xml";
/// A pool channel isn't let go sooner than this after it was taken (its IDN route may
/// not be made yet, and the device list shows a new subscription a moment later).
const GRACE_S: u64 = 60;
/// The device program's IDN receiver (`idn-aoip --name`, as the Node starts it).
pub(super) const AOIP_NAME: &str = "AoIP";

/// One change at a time (two clicks at once took one channel twice).
static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A receive channel the pool subscribed: to `channel` of `device`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct In {
    k: u16,
    device: String,
    channel: String,
    at: u64,
}

/// A transmit channel the pool feeds: from `tx` channel `tx_ch` (from 0).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Out {
    k: u16,
    tx: String,
    tx_ch: u16,
    at: u64,
    /// The device inputs it was given to (device name, input from 1): it is let go only
    /// when each is seen read and not on it (bug run M4-1, M5-1: a device switched off,
    /// or one whose inputs aren't read, counted as not using it).
    to: Vec<(String, u16)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Pool {
    ins: Vec<In>,
    outs: Vec<Out>,
}

impl Pool {
    /// The table as saved (times ahead of now taken as now: a clock set back would hold
    /// them for good; bug run M4-4).
    fn load(path: &std::path::Path) -> Pool {
        let mut p = Pool::load_raw(path);
        let now = idn_common::epoch::secs();
        for i in &mut p.ins {
            i.at = i.at.min(now);
        }
        for o in &mut p.outs {
            o.at = o.at.min(now);
        }
        p
    }

    fn load_raw(path: &std::path::Path) -> Pool {
        // (One that can't be read is set aside, not saved over with an empty table: every
        // pool subscription lost; bug review 3.)
        let doc = match idn_common::fsutil::load_saved(path, |b| {
            std::str::from_utf8(b)
                .ok()
                .and_then(idn_open_protocol::xml::parse)
        }) {
            Ok(Some(doc)) => doc,
            Ok(None) => return Pool::default(),
            Err(e) => {
                idn_open_protocol::log::log(&format!("network audio pools: {e}"));
                return Pool::default();
            }
        };
        let n = |e: &Element, k: &str| e.attr(k).and_then(|v| v.parse::<u64>().ok());
        let s = |e: &Element, k: &str| e.attr(k).unwrap_or("").to_owned();
        Pool {
            ins: doc
                .children_named("in")
                .filter_map(|e| {
                    Some(In {
                        k: u16::try_from(n(e, "k")?).ok()?,
                        device: s(e, "device"),
                        channel: s(e, "channel"),
                        at: n(e, "at").unwrap_or(0),
                    })
                })
                .collect(),
            outs: doc
                .children_named("out")
                .filter_map(|e| {
                    Some(Out {
                        k: u16::try_from(n(e, "k")?).ok()?,
                        tx: s(e, "tx"),
                        tx_ch: u16::try_from(n(e, "tx_ch")?).ok()?,
                        at: n(e, "at").unwrap_or(0),
                        to: e
                            .children_named("to")
                            .filter_map(|t| {
                                Some((s(t, "device"), u16::try_from(n(t, "ch")?).ok()?))
                            })
                            .collect(),
                    })
                })
                .collect(),
        }
    }

    fn element(&self) -> Element {
        let mut e = Element::new("pool");
        for i in &self.ins {
            e = e.with_child(
                Element::new("in")
                    .with_attr("k", &i.k.to_string())
                    .with_attr("device", &i.device)
                    .with_attr("channel", &i.channel)
                    .with_attr("at", &i.at.to_string()),
            );
        }
        for o in &self.outs {
            let mut x = Element::new("out")
                .with_attr("k", &o.k.to_string())
                .with_attr("tx", &o.tx)
                .with_attr("tx_ch", &o.tx_ch.to_string())
                .with_attr("at", &o.at.to_string());
            for (d, ch) in &o.to {
                x = x.with_child(
                    Element::new("to")
                        .with_attr("device", d)
                        .with_attr("ch", &ch.to_string()),
                );
            }
            e = e.with_child(x);
        }
        e
    }

    fn save(&self, path: &std::path::Path) -> Result<(), String> {
        idn_common::fsutil::write_atomic(path, self.element().to_xml().as_bytes())
            .map_err(|e| format!("the pool's table: {e}"))
    }
}

/// A channel's subscription as a device list gives it: (from_device, from_channel).
fn subscribed(d: &Element, n: u16) -> Option<(String, String)> {
    let c = d
        .children_named("rx")
        .find(|c| c.attr("n").and_then(|v| v.parse::<u16>().ok()) == Some(n))?;
    let dev = c.attr("from_device").filter(|v| !v.is_empty())?;
    Some((
        dev.to_owned(),
        c.attr("from_channel").unwrap_or("").to_owned(),
    ))
}

/// Whether a device's inputs were read (listed with its receive channels, no error): until
/// then no input of it is taken as free or as no longer the pool's (bug run N8-3).
fn inputs_read(d: &Element) -> bool {
    d.attr("error").is_none() && d.children_named("rx").next().is_some()
}

/// Whether a device's outputs were read (listed with its transmit channels, no error): until
/// then an output's name isn't known, and no listener is seen on it (bug run N3-1: every
/// pool output let go while the device was being read again after a restart).
fn outputs_read(d: &Element) -> bool {
    d.attr("error").is_none() && d.children_named("tx").next().is_some()
}

/// An output's name as subscriptions name it (its given name, else its number).
fn tx_name(d: &Element, n: u16) -> String {
    d.children_named("tx")
        .find(|c| c.attr("n").and_then(|v| v.parse::<u16>().ok()) == Some(n))
        .and_then(|c| c.attr("name").filter(|v| !v.is_empty()))
        .map_or_else(|| n.to_string(), str::to_owned)
}

impl NodeCore {
    /// The device's IDN source, as IDN receivers route it.
    pub(super) fn aoip_source() -> String {
        format!("{} ({AOIP_NAME})", idn_open_protocol::discovery::hostname())
    }

    /// The pool's table for the report (`<pool source in out>` with its entries), while the
    /// network audio device is on.
    pub(super) fn netaudio_pool_element(&self) -> Element {
        let s = self.settings();
        Pool::load(&self.dir.join(FILE))
            .element()
            .with_attr("source", &Self::aoip_source())
            .with_attr("in", &s.netaudio.channels.unwrap_or(64).to_string())
            .with_attr("out", &s.netaudio.channels.unwrap_or(64).to_string())
    }

    /// `<netaudio_do op="pool_in|pool_out|pool_sweep" …/>`: what was done, and for
    /// `pool_in` the channel and source to route from.
    pub(super) fn netaudio_pool(&self, d: &Element) -> Result<Vec<(String, String)>, String> {
        let s = self.settings();
        if !s.netaudio.on {
            return Err("the network audio device isn't on here".into());
        }
        if s.netaudio.read_only {
            return Err("this machine's network audio is read only (its setting)".into());
        }
        let _turn = TURN.lock_ok();
        let path = self.dir.join(FILE);
        let mut pool = Pool::load(&path);
        let doc = self.netaudio_devices()?;
        let own = doc
            .children_named("device")
            .find(|e| e.attr("this") == Some("true"))
            .ok_or("this machine's network audio device isn't in its own list yet")?;
        let own_t = target_of(own)?;
        let own_name = own.attr("name").unwrap_or("").to_owned();
        let a = |k: &str| d.attr(k).unwrap_or("").trim().to_owned();
        let out = match a("op").as_str() {
            "pool_in" => {
                let (src, _) = find_device(&doc, &a("device"))?;
                if src.attr("this") == Some("true") {
                    return Err("that is this machine's own network audio device: its sound is IDN's already".into());
                }
                // The desk in the device's channels takes its inputs as its own strips: none
                // for the pool (bug run M4-2: a strip changed and later went silent).
                if s.netaudio.desk {
                    return Err("the desk works in this machine's network audio device: its inputs are the desk's".into());
                }
                // Its inputs not read yet: one looks free that isn't (bug run N8-3).
                if !inputs_read(own) {
                    return Err(
                        "this machine's network audio device isn't read yet: try again in a minute"
                            .into(),
                    );
                }
                let src_name = src.attr("name").unwrap_or("").to_owned();
                let channel = a("tx_channel");
                // Only an output its list offers (K.5).
                let offered = src.children_named("tx").count() == 0
                    || src.children_named("tx").any(|c| {
                        c.attr("name") == Some(channel.as_str())
                            || c.attr("n") == Some(channel.as_str())
                    });
                if channel.is_empty() || !offered {
                    return Err(format!("{src_name} has no output {channel:?}"));
                }
                // The pool's entry for it, while it is still so on the device (or too new for
                // its list to show; bug run L1-3: moved by hand, the other sound was used);
                // a stale one dropped. Used again: its minute's grace starts again (L1-2:
                // the sweep right after let it go).
                let fresh = |at: u64| idn_common::epoch::secs().saturating_sub(at) < GRACE_S;
                let found = pool
                    .ins
                    .iter()
                    .position(|i| i.device == src_name && i.channel == channel);
                let found = match found {
                    Some(n)
                        if fresh(pool.ins[n].at)
                            || subscribed(own, pool.ins[n].k)
                                == Some((src_name.clone(), channel.clone())) =>
                    {
                        Some(n)
                    }
                    Some(n) => {
                        pool.ins.remove(n);
                        None
                    }
                    None => None,
                };
                let k = if let Some(n) = found {
                    pool.ins[n].at = idn_common::epoch::secs();
                    pool.save(&path)?;
                    pool.ins[n].k
                } else if let Some(k) = (1..=own_t.rx).find(|&k| {
                    subscribed(own, k).as_ref() == Some(&(src_name.clone(), channel.clone()))
                }) {
                    // On one of its channels already (by hand): used, never undone.
                    k
                } else {
                    let k = (1..=own_t.rx)
                        .find(|&k| {
                            subscribed(own, k).is_none() && !pool.ins.iter().any(|i| i.k == k)
                        })
                        .ok_or_else(|| {
                            format!(
                                "all {} of this machine's network audio inputs are in use",
                                own_t.rx
                            )
                        })?;
                    let id = next_id();
                    let (req, opcode) = subscribe_request(&own_t, id, k, &channel, &src_name)?;
                    let reply = ask(self.netaudio_interface(), own_t.arc, &req)?;
                    control::write_ok(&reply, id, opcode)?;
                    pool.ins.retain(|i| i.k != k);
                    pool.ins.push(In {
                        k,
                        device: src_name.clone(),
                        channel: channel.clone(),
                        at: idn_common::epoch::secs(),
                    });
                    pool.save(&path)?;
                    self.log(&format!(
                        "network audio: pool input {k} ← {channel}@{src_name} (from the Site Manager)"
                    ));
                    k
                };
                vec![
                    ("did".into(), format!("{channel}@{src_name} on input {k}")),
                    ("k".into(), k.to_string()),
                    ("source".into(), Self::aoip_source()),
                ]
            }
            "pool_out" => {
                let (dst, t) = find_device(&doc, &a("device"))?;
                let dst_name = dst.attr("name").unwrap_or("").to_owned();
                let ch = a("channel")
                    .parse::<u16>()
                    .ok()
                    .filter(|&n| (1..=t.rx).contains(&n))
                    .ok_or_else(|| format!("channel: 1 to {} on {dst_name}", t.rx))?;
                let tx = a("tx");
                let tx_ch = a("tx_ch")
                    .parse::<u16>()
                    .map_err(|_| "tx_ch: the transmitter's channel, from 0".to_owned())?;
                if tx.is_empty() {
                    return Err("which IDN transmitter?".into());
                }
                if dst.attr("this") == Some("true") {
                    return Err("that is this machine's own network audio device".into());
                }
                // The desk in the device's channels writes its outputs (IDN's routes to it
                // wait): none for the pool (bug run L1-4).
                if s.netaudio.desk {
                    return Err("the desk works in this machine's network audio device: its outputs are the desk's".into());
                }
                if !outputs_read(own) {
                    return Err(
                        "this machine's network audio device isn't read yet: try again in a minute"
                            .into(),
                    );
                }
                let routes = self.routes.lock_ok().routes(AOIP_NAME);
                // The pool's output for it, while its IDN route is still the pool's (L1-3);
                // used again, its grace starts again (L1-2).
                let reused = pool
                    .outs
                    .iter()
                    .position(|o| o.tx == tx && o.tx_ch == tx_ch)
                    .and_then(|n| {
                        let o = &pool.outs[n];
                        let ours = routes
                            .iter()
                            .any(|r| r.ch + 1 == o.k && r.tx == o.tx && r.tx_ch == o.tx_ch);
                        if ours {
                            Some(n)
                        } else {
                            pool.outs.remove(n);
                            None
                        }
                    })
                    .map(|n| {
                        pool.outs[n].at = idn_common::epoch::secs();
                        pool.outs[n].k
                    });
                if reused.is_some() {
                    pool.save(&path)?;
                }
                let k = match reused {
                    Some(k) => k,
                    None => {
                        // Free: not the pool's, no IDN route into it, no device takes it.
                        let used_by_device = |k: u16| {
                            let name = tx_name(own, k);
                            doc.children_named("device")
                                .filter(|e| e.attr("this") != Some("true"))
                                .any(|e| {
                                    e.children_named("rx").any(|c| {
                                        c.attr("from_device") == Some(own_name.as_str())
                                            && c.attr("from_channel") == Some(name.as_str())
                                    })
                                })
                        };
                        let k = (1..=own_t.tx)
                            .find(|&k| {
                                !pool.outs.iter().any(|o| o.k == k)
                                    && !routes.iter().any(|r| r.ch + 1 == k)
                                    && !used_by_device(k)
                            })
                            .ok_or_else(|| {
                                format!(
                                    "all {} of this machine's network audio outputs are in use",
                                    own_t.tx
                                )
                            })?;
                        self.apply_routes(
                            &[RouteChange {
                                receiver: AOIP_NAME.into(),
                                ch: k - 1,
                                to: Some((tx.clone(), tx_ch)),
                            }],
                            true,
                        )
                        .map_err(|e| e.to_string())?;
                        pool.outs.push(Out {
                            k,
                            tx: tx.clone(),
                            tx_ch,
                            at: idn_common::epoch::secs(),
                            to: Vec::new(),
                        });
                        // Not kept: the route undone (L1-7: a route left in no table).
                        if let Err(e) = pool.save(&path) {
                            self.pool_let_go_out(&mut pool, k);
                            return Err(e);
                        }
                        k
                    }
                };
                // The device's input subscribed to that output (taken back if refused).
                let id = next_id();
                let name = tx_name(own, k);
                let sent =
                    subscribe_request(&t, id, ch, &name, &own_name).and_then(|(req, opcode)| {
                        let reply = ask(self.netaudio_interface(), t.arc, &req)?;
                        control::write_ok(&reply, id, opcode)
                    });
                if let Err(e) = sent {
                    if reused.is_none() && self.pool_let_go_out(&mut pool, k) {
                        pool.save(&path)?;
                    }
                    return Err(e);
                }
                // Which device input it feeds, kept with it (M4-1, M5-1).
                if let Some(o) = pool.outs.iter_mut().find(|o| o.k == k) {
                    if !o.to.iter().any(|(d, c)| *d == dst_name && *c == ch) {
                        o.to.push((dst_name.clone(), ch));
                    }
                    pool.save(&path)?;
                }
                self.log(&format!(
                    "network audio: {dst_name} {ch} ← {tx} {} through pool output {k} (from the Site Manager)",
                    tx_ch + 1
                ));
                vec![(
                    "did".into(),
                    format!("{dst_name} {ch} ← {tx} {} (output {k})", tx_ch + 1),
                )]
            }
            "pool_sweep" => {
                // The receive channels some IDN route still takes (the Site Manager's view).
                let in_use: Vec<u16> = a("in_use")
                    .split(',')
                    .filter_map(|v| v.trim().parse().ok())
                    .collect();
                let old = |at: u64| idn_common::epoch::secs().saturating_sub(at) >= GRACE_S;
                let mut freed = Vec::new();
                let mut failed: Vec<String> = Vec::new();
                // Its inputs not read (listed before its first read, or the read failed):
                // the pool's inputs kept as they are (dropped unread, a subscription stayed
                // on the device with no entry; bug run N8-3).
                let ins = if inputs_read(own) {
                    pool.ins.clone()
                } else {
                    Vec::new()
                };
                for i in ins {
                    if in_use.contains(&i.k) || !old(i.at) {
                        continue;
                    }
                    // Undone only while it is still the pool's subscription; one that fails
                    // kept for the next sweep, the others going on (bug run L1-6).
                    if subscribed(own, i.k) == Some((i.device.clone(), i.channel.clone())) {
                        let id = next_id();
                        let done =
                            unsubscribe_request(&own_t, id, i.k).and_then(|(req, opcode)| {
                                let reply = ask(self.netaudio_interface(), own_t.arc, &req)?;
                                control::write_ok(&reply, id, opcode)
                            });
                        if let Err(e) = done {
                            failed.push(format!("input {}: {e}", i.k));
                            // Not answering: the rest wait for the next sweep (bug run M4-3:
                            // 2 s each, the clicks held behind them).
                            if e.contains("no answer") {
                                break;
                            }
                            continue;
                        }
                    }
                    pool.ins.retain(|x| x.k != i.k);
                    freed.push(format!("input {}", i.k));
                }
                // Its outputs not read: the pool's outputs kept as they are (N3-1).
                let outs = if outputs_read(own) {
                    pool.outs.clone()
                } else {
                    Vec::new()
                };
                for o in outs {
                    if !old(o.at) {
                        continue;
                    }
                    let name = tx_name(own, o.k);
                    // Each device it was given to: still on it, or not known (not listed,
                    // not read, its inputs not read): kept. Seen read and not on it: that
                    // one let go (bug run M4-1, M5-1). An entry from before `to` was kept
                    // goes by the old rule (no listed device on it).
                    let on_it = |e: &Element, ch: Option<u16>| {
                        e.children_named("rx").any(|c| {
                            ch.is_none_or(|ch| c.attr("n") == Some(ch.to_string().as_str()))
                                && c.attr("from_device") == Some(own_name.as_str())
                                && c.attr("from_channel") == Some(name.as_str())
                        })
                    };
                    let mut still = Vec::new();
                    for (dev, ch) in &o.to {
                        let seen = doc.children_named("device").find(|e| {
                            e.attr("name") == Some(dev.as_str()) && e.attr("this") != Some("true")
                        });
                        let read = seen.is_some_and(inputs_read);
                        if !read || seen.is_some_and(|e| on_it(e, Some(*ch))) {
                            still.push((dev.clone(), *ch));
                        }
                    }
                    let taken = if o.to.is_empty() {
                        doc.children_named("device")
                            .filter(|e| e.attr("this") != Some("true"))
                            .any(|e| on_it(e, None))
                    } else {
                        // Another device subscribed to it by hand meanwhile: kept too.
                        !still.is_empty()
                            || doc
                                .children_named("device")
                                .filter(|e| e.attr("this") != Some("true"))
                                .any(|e| on_it(e, None))
                    };
                    if taken
                        && still.len() != o.to.len()
                        && let Some(x) = pool.outs.iter_mut().find(|x| x.k == o.k)
                    {
                        x.to = still.clone();
                        freed.push(format!("output {}'s gone listener", o.k));
                    }
                    if !taken {
                        if self.pool_let_go_out(&mut pool, o.k) {
                            freed.push(format!("output {}", o.k));
                        } else {
                            failed.push(format!("output {}: its route not cleared", o.k));
                        }
                    }
                }
                if !failed.is_empty() {
                    self.log(&format!(
                        "network audio: pool channels not let go: {}",
                        failed.join("; ")
                    ));
                }
                if !freed.is_empty() {
                    pool.save(&path)?;
                    self.log(&format!(
                        "network audio: pool {} let go (nothing uses them)",
                        freed.join(", ")
                    ));
                }
                if !failed.is_empty() && freed.is_empty() {
                    return Err(format!("not let go: {}", failed.join("; ")));
                }
                vec![("did".into(), format!("{} let go", freed.len()))]
            }
            other => return Err(format!("{other:?}: not a pool change")),
        };
        Ok(out)
    }

    /// Output `k` out of the pool: its IDN route cleared while it is still the pool's.
    /// Whether it went (kept when its route couldn't be cleared; bug run L1-7).
    fn pool_let_go_out(&self, pool: &mut Pool, k: u16) -> bool {
        if let Some(o) = pool.outs.iter().find(|o| o.k == k).cloned() {
            let ours = self
                .routes
                .lock_ok()
                .routes(AOIP_NAME)
                .iter()
                .any(|r| r.ch + 1 == k && r.tx == o.tx && r.tx_ch == o.tx_ch);
            if ours
                && self
                    .apply_routes(
                        &[RouteChange {
                            receiver: AOIP_NAME.into(),
                            ch: k - 1,
                            to: None,
                        }],
                        true,
                    )
                    .is_err()
            {
                return false;
            }
        }
        pool.outs.retain(|o| o.k != k);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_kept_as_written() {
        let dir = idn_common::testing::TempDir::new("idn-pool");
        let path = dir.join(FILE);
        let p = Pool {
            ins: vec![In {
                k: 3,
                device: "Stage-Box".into(),
                channel: "Vox 1".into(),
                at: 7,
            }],
            outs: vec![Out {
                k: 1,
                tx: "FOH (Mixer)".into(),
                tx_ch: 4,
                at: 9,
                to: vec![("Amp-1".into(), 2), ("Amp-2".into(), 1)],
            }],
        };
        p.save(&path).unwrap();
        assert_eq!(Pool::load(&path), p);
    }

    #[test]
    fn subscriptions_and_names_from_a_device_list() {
        let d = idn_open_protocol::xml::parse(
            r#"<device name="A"><rx n="1" name="x" status="0"/><rx n="2" name="y" status="9" from_device="B" from_channel="Out 3"/><tx n="1" name="Left"/><tx n="2"/></device>"#,
        )
        .unwrap();
        assert_eq!(subscribed(&d, 1), None);
        assert_eq!(subscribed(&d, 2), Some(("B".into(), "Out 3".into())));
        assert_eq!(tx_name(&d, 1), "Left");
        assert_eq!(tx_name(&d, 2), "2");
    }

    /// A device listed before its first read, or whose read failed, hasn't its inputs read
    /// (bug run N8-3: the pool's inputs neither taken as free nor dropped then).
    #[test]
    fn inputs_count_as_read_only_when_listed_without_an_error() {
        let p = |x: &str| idn_open_protocol::xml::parse(x).unwrap();
        assert!(inputs_read(&p(
            r#"<device name="A"><rx n="1" name="x"/></device>"#
        )));
        assert!(!inputs_read(&p(r#"<device name="A" rx_count="8"/>"#)));
        assert!(outputs_read(&p(
            r#"<device name="A"><tx n="1" name="x"/></device>"#
        )));
        assert!(!outputs_read(&p(
            r#"<device name="A"><rx n="1" name="x"/></device>"#
        )));
        assert!(!inputs_read(&p(
            r#"<device name="A" error="no answer"><rx n="1" name="x"/></device>"#
        )));
    }
}
