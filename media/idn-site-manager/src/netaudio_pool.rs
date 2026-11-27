//! The pool from the Site Manager (roadmap R.32 stage 1c, idn-aoip-controller.md K.3,
//! K.12): a route between a network device and IDN in one click. The Node of the machine
//! running the network audio device takes one of its channels (`pool_in` / `pool_out`);
//! for a device → IDN route this then routes the IDN receiver from it. Pool channels
//! nothing uses are let go (`pool_sweep`) every 30 s, not in the Site Manager's first
//! 3 minutes; the routes counted are every machine's last known ones (kept on disk until it
//! reports again) and the saved routing presets'.

use super::*;
use idn_open_protocol::node::routing::RouteChange;
use idn_open_protocol::trust::NodeReport;
use idn_open_protocol::xml::Element;
use serde_json::Value;

/// The machines whose network audio device can run the pool for `device`, in a fixed order
/// (by fingerprint; bug run M5-4: the reports' order was a hash's): each one's own list has
/// it, its own device read too (or only the one named). For an output (`out`), not a
/// machine whose desk works in its device (its outputs are the desk's).
fn pool_machines(reports: &[NodeReport], device: &str, machine: &str, out: bool) -> Vec<String> {
    let mut v: Vec<String> = reports
        .iter()
        .filter(|r| machine.is_empty() || r.fingerprint == machine)
        .filter(|r| !out || r.report.child("netaudio").and_then(|n| n.attr("desk")) != Some("true"))
        .filter(|r| {
            r.report.child("network_devices").is_some_and(|l| {
                l.children_named("device")
                    .any(|d| d.attr("this") == Some("true"))
                    && l.children_named("device")
                        .any(|d| d.attr("this") != Some("true") && netaudio::named(d, device))
            })
        })
        .map(|r| r.fingerprint.clone())
        .collect();
    v.sort();
    v
}

/// The first of [`pool_machines`] (for tests and the "in" order).
#[cfg(test)]
fn pool_machine(reports: &[NodeReport], device: &str, machine: &str) -> Result<String, String> {
    pool_machines(reports, device, machine, false)
        .into_iter()
        .next()
        .ok_or_else(|| format!("no machine running a network audio device reads {device} now"))
}

/// Asks the Node `fp` for a pool change: its answer's attributes, or why not.
/// Why a machine didn't do it, and whether another machine may (it refused); a machine
/// not answering in time may still be doing it, and an IDN route that failed fails on
/// any (bug run N2-6, N6-20: the next machine took the same channel, or another pool
/// channel each).
struct Failed(String, bool);

impl From<&str> for Failed {
    fn from(e: &str) -> Self {
        Failed(e.to_owned(), false)
    }
}

fn ask(a: &Authority, fp: &str, e: Element) -> Result<Element, Failed> {
    match a.netaudio_node(fp, e, Duration::from_secs(8)) {
        Ok(reply) if reply.attr("ok") == Some("true") => Ok(reply),
        Ok(reply) => Err(Failed(
            reply
                .attr("error")
                .unwrap_or("the machine refused")
                .to_owned(),
            true,
        )),
        Err(e) => Err(Failed(e.to_string(), false)),
    }
}

/// `POST /api/v1/network-devices/pool`: `{dir: "in", device, tx_channel, fingerprint,
/// receiver, ch}` (a device's output to an IDN receiver's channel, `ch` from 0) or `{dir:
/// "out", tx, tx_ch, device, channel}` (an IDN transmitter's channel, from 0, to a device's
/// input, from 1). What was done.
pub(super) fn request(a: &Authority, v: &Value) -> Result<String, String> {
    let t = |k: &str| {
        v.get(k)
            .map(|x| match x {
                Value::String(s) => s.trim().to_owned(),
                Value::Number(n) => n.to_string(),
                _ => String::new(),
            })
            .unwrap_or_default()
    };
    let device = t("device");
    if device.is_empty() {
        return Err("which device?".into());
    }
    let reports = a.node_reports();
    // Read only on any machine: read only for the site (K.5).
    if netaudio::read_only(&reports) {
        return Err(
            "the network's devices are read only on this site (a machine's network audio setting)"
                .into(),
        );
    }
    // A machine's own IDN device isn't fed from here, whichever machine is asked (bug run
    // L2-7, as K5-3 for the stage 1b changes); said as such, before which machine (lab
    // Alpha84 533(g): "no machine … reads" was said).
    if t("dir") == "out" && netaudio::own_device(&reports, &device) {
        return Err(format!(
            "{device} is a machine's own IDN network audio device: route to it in IDN"
        ));
    }
    let machines = pool_machines(&reports, &device, &t("machine"), t("dir") == "out");
    if machines.is_empty() {
        return Err(format!(
            "no machine running a network audio device{} reads {device} now",
            if t("dir") == "out" {
                " (without its desk in it)"
            } else {
                ""
            }
        ));
    }
    // Each in turn until one does it (M5-4: one refusing, the next may not); only after a
    // refusal (N2-6).
    let mut why = String::new();
    for fp in &machines {
        match request_on(a, v, fp, &device) {
            Ok(did) => return Ok(did),
            Err(Failed(e, true)) => why = e,
            Err(Failed(e, false)) => return Err(e),
        }
    }
    Err(why)
}

/// [`request`] on the machine `fp`.
fn request_on(a: &Authority, v: &Value, fp: &str, device: &str) -> Result<String, Failed> {
    let t = |k: &str| {
        v.get(k)
            .map(|x| match x {
                Value::String(s) => s.trim().to_owned(),
                Value::Number(n) => n.to_string(),
                _ => String::new(),
            })
            .unwrap_or_default()
    };
    let (fp, device) = (fp.to_owned(), device.to_owned());
    let did = match t("dir").as_str() {
        "in" => {
            let rx_fp = t("fingerprint");
            let receiver = t("receiver");
            let ch: u16 = t("ch")
                .parse()
                .ok()
                .filter(|&c| c < 256)
                .ok_or("ch: the receiver's channel, 0 to 255")?;
            if rx_fp.is_empty() || receiver.is_empty() {
                return Err("which IDN receiver?".into());
            }
            let reply = ask(
                a,
                &fp,
                Element::new("netaudio_do")
                    .with_attr("op", "pool_in")
                    .with_attr("device", &device)
                    .with_attr("tx_channel", &t("tx_channel")),
            )?;
            let k: u16 = reply
                .attr("k")
                .and_then(|k| k.parse().ok())
                .filter(|&k| k >= 1)
                .ok_or("the machine gave no channel")?;
            let source = reply.attr("source").unwrap_or("").to_owned();
            let change = RouteChange {
                receiver: receiver.clone(),
                ch,
                to: Some((source.clone(), k - 1)),
            };
            // (The pool input just taken is let go by the sweep when no route uses it.)
            match a.route_node(&rx_fp, &[change], Duration::from_secs(5)) {
                Ok(r) if r.attr("ok") == Some("true") => {}
                Ok(r) => {
                    return Err(r
                        .attr("error")
                        .unwrap_or("the receiver's machine refused")
                        .into());
                }
                Err(e) => return Err(Failed(e.to_string(), false)),
            }
            format!(
                "{} → {receiver} channel {} (through {source} {k})",
                reply.attr("did").unwrap_or(""),
                ch + 1
            )
        }
        "out" => {
            let reply = ask(
                a,
                &fp,
                Element::new("netaudio_do")
                    .with_attr("op", "pool_out")
                    .with_attr("device", &device)
                    .with_attr("channel", &t("channel"))
                    .with_attr("tx", &t("tx"))
                    .with_attr("tx_ch", &t("tx_ch")),
            )?;
            reply.attr("did").unwrap_or("done").to_owned()
        }
        other => return Err(Failed(format!("dir {other:?}: in or out"), false)),
    };
    // (No sweep here: the reports don't carry the new route for a moment, and the channel
    // just used could go with it (bug run L2-1); the watch thread's every 30 s does.)
    Ok(did)
}

/// Each machine's routes from the pools' sources, kept a while after it goes (bug run
/// L2-2: a machine offline for 30 s lost its pool channels, its routes then silent or on
/// another device's sound when it came back).
/// Kept on disk (`pool-routes.json`) until the machine reports again (bug run M5-2: a PC off
/// for 30 min, or the Site Manager restarted, and its pool inputs were let go).
type Remembered = std::collections::BTreeMap<String, (u64, Vec<(String, u16)>)>;
static REMEMBERED: std::sync::Mutex<Option<Remembered>> = std::sync::Mutex::new(None);
/// A machine gone this long (removed from the site, never back) no longer holds channels.
const REMEMBER_S: u64 = 90 * 24 * 3600;
const ROUTES_FILE: &str = "pool-routes.json";

/// (One that can't be read is set aside and said: read as none, the next sweep wrote it
/// over and released the pool channels of machines briefly away; bug run 4.)
fn load_remembered(dir: &std::path::Path) -> Remembered {
    match idn_common::fsutil::load_saved(&dir.join(ROUTES_FILE), |b| {
        serde_json::from_slice::<Remembered>(b).ok()
    }) {
        Ok(r) => r.unwrap_or_default(),
        Err(why) => {
            idn_open_protocol::log::log(&format!("network audio pools: {why}"));
            Remembered::new()
        }
    }
}
/// No sweep sooner than this after the Site Manager starts (every machine reconnected).
const SETTLE: Duration = Duration::from_secs(180);
static FIRST: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
static SWEEPING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// [`sweep`] on a thread of its own, one at a time (bug run L2-5: a Node that didn't answer
/// held the watch thread up to 8 s a machine).
/// `dir`: the Site Manager's folder (routes remembered there); `held`: the saved routing
/// presets' routes (source, channel from 0), which count as in use (bug run M5-3).
pub(super) fn sweep_later(a: &Authority, dir: &std::path::Path, held: Vec<(String, u16)>) {
    use std::sync::atomic::Ordering;
    let first = *FIRST.get_or_init(std::time::Instant::now);
    if first.elapsed() < SETTLE || SWEEPING.swap(true, Ordering::Relaxed) {
        return;
    }
    let (a, dir) = (a.clone(), dir.to_owned());
    let spawned = std::thread::Builder::new()
        .name("idn-site-pool-sweep".into())
        .spawn(move || {
            sweep(&a, &dir, &held);
            SWEEPING.store(false, Ordering::Relaxed);
        });
    if spawned.is_err() {
        SWEEPING.store(false, Ordering::Relaxed);
    }
}

/// Each pool's channels some IDN route still takes, told to its Node, which lets the
/// others go (a minute after they were taken at the soonest).
pub(super) fn sweep(a: &Authority, dir: &std::path::Path, held_by_presets: &[(String, u16)]) {
    let reports = a.node_reports();
    // Not on a read-only site (K.5).
    if netaudio::read_only(&reports) {
        return;
    }
    // Every machine's routes now, and those of machines gone a short while.
    let routes = {
        let mut held = REMEMBERED.lock_ok();
        let held = held.get_or_insert_with(|| load_remembered(dir));
        let before = held.clone();
        let now = idn_common::epoch::secs();
        for r in &reports {
            held.insert(r.fingerprint.clone(), (now, routes_of(r)));
        }
        held.retain(|_, (at, _)| now.saturating_sub(*at) < REMEMBER_S);
        // Saved when the routes changed (not each 30 s for the times alone).
        let changed = held.len() != before.len()
            || held
                .iter()
                .any(|(k, (_, v))| before.get(k).is_none_or(|(_, w)| w != v));
        if changed
            && let Ok(text) = serde_json::to_string(&*held)
            && let Err(e) =
                idn_common::fsutil::write_atomic(&dir.join(ROUTES_FILE), text.as_bytes())
        {
            idn_open_protocol::log::log(&format!("site manager: {ROUTES_FILE}: {e}"));
        }
        let mut all: Vec<(String, u16)> = held.values().flat_map(|(_, v)| v.clone()).collect();
        all.extend(held_by_presets.iter().cloned());
        all
    };
    for r in &reports {
        let Some(pool) = r
            .report
            .child("network_devices")
            .and_then(|l| l.child("pool"))
        else {
            continue;
        };
        if pool.children.is_empty() {
            continue;
        }
        let source = pool.attr("source").unwrap_or("");
        let in_use = in_use(&routes, source);
        let e = Element::new("netaudio_do")
            .with_attr("op", "pool_sweep")
            .with_attr(
                "in_use",
                &in_use
                    .iter()
                    .map(u16::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
            );
        // Said when it fails (L2-5: a pool whose sweeps fail never lets a channel go).
        if let Err(Failed(e, _)) = ask(a, &r.fingerprint, e) {
            idn_open_protocol::log::log(&format!(
                "site manager: network audio pool on {}: {e}",
                r.report.attr("name").unwrap_or(&r.fingerprint)
            ));
        }
    }
}

/// A machine's routes as (source, its channel from 0).
fn routes_of(r: &NodeReport) -> Vec<(String, u16)> {
    r.report
        .children_named("audio_rx")
        .flat_map(|x| x.children_named("route"))
        .filter_map(|rt| Some((rt.attr("tx")?.to_owned(), rt.attr("tx_ch")?.parse().ok()?)))
        .collect()
}

/// The channels (from 1) of `source` some receiver routes from.
fn in_use(routes: &[(String, u16)], source: &str) -> Vec<u16> {
    let mut v: Vec<u16> = routes
        .iter()
        .filter(|(tx, _)| tx == source)
        .map(|(_, c)| c + 1)
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(fp: &str, xml: &str) -> NodeReport {
        NodeReport {
            fingerprint: fp.into(),
            report: idn_open_protocol::xml::parse(xml).unwrap(),
            age: Duration::ZERO,
            address: None,
        }
    }

    #[test]
    fn the_pool_machine_and_the_channels_in_use() {
        let rs = vec![
            report(
                "A",
                r#"<idn_node_report><network_devices><device name="Box" instance="Box"/></network_devices></idn_node_report>"#,
            ),
            report(
                "B",
                r#"<idn_node_report><network_devices><device name="B-IDN" this="true"/><device name="Box" instance="Box"/></network_devices><audio_rx name="Desk"><route ch="0" tx="b (AoIP)" tx_ch="4"/><route ch="1" tx="other (X)" tx_ch="0"/></audio_rx></idn_node_report>"#,
            ),
            report(
                "C",
                r#"<idn_node_report><audio_rx name="Rec"><route ch="3" tx="b (AoIP)" tx_ch="4"/><route ch="4" tx="b (AoIP)" tx_ch="0"/></audio_rx></idn_node_report>"#,
            ),
        ];
        // A reads the device but runs none of its own: B does the pool.
        assert_eq!(pool_machine(&rs, "Box", "").unwrap(), "B");
        assert!(pool_machine(&rs, "Box", "A").is_err());
        assert!(pool_machine(&rs, "Nothing", "").is_err());
        let routes: Vec<_> = rs.iter().flat_map(routes_of).collect();
        assert_eq!(in_use(&routes, "b (AoIP)"), vec![1, 5]);
    }
}
