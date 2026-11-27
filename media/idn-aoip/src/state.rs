//! What a device keeps across restarts (idn-aoip-device.md §1.4): its channels' names,
//! its receive channels' subscriptions, its name when a controller renamed it, its
//! latency and the multicast flows controllers made (§6.6). One file, written whole
//! (§1.4: a crash never leaves half of it).

use idn_aoip_device::device::{Device, Subscription, status};
use serde_json::{Value, json};
use std::net::Ipv4Addr;
use std::path::Path;
use std::sync::Mutex;

/// What a save keeps beside the device itself (this run's, set at start).
#[derive(Default)]
struct Kept {
    /// The latency the device was started with (the Node's setting): a saved latency is
    /// taken only while that setting is the one it was saved under (bug run N7-1: a new
    /// setting was overwritten by the saved one at every start).
    started_latency_ns: Option<u32>,
    /// Saved channels past the ones it has now (fewer channels set): kept for when they
    /// come back (N7-6: 64 → 16 → 64 lost channels 17–64's names and subscriptions).
    rx_beyond: Vec<Value>,
    tx_beyond: Vec<Value>,
    /// The name it wanted and the "-2" it took for a clash at start: the wanted one saved,
    /// tried again next start (N3-6: the clash name kept for good).
    clash: Option<(String, String)>,
}
static KEPT: Mutex<Option<Kept>> = Mutex::new(None);

fn kept<R>(f: impl FnOnce(&mut Kept) -> R) -> R {
    use idn_common::sync::LockOk as _;
    f(KEPT.lock_ok().get_or_insert_with(Kept::default))
}

/// The latency this run was started with (before [`load`]).
pub fn started_with_latency(ns: u32) {
    kept(|k| k.started_latency_ns = Some(ns));
}

/// The name taken at start for a clash (`used`) instead of `wanted`.
pub fn clash_name(wanted: &str, used: &str) {
    kept(|k| k.clash = Some((wanted.to_owned(), used.to_owned())));
}

/// The device as it is worth keeping.
pub fn to_json(d: &Device) -> Value {
    let flows: Vec<Value> = d
        .tx_flows
        .flows
        .iter()
        .enumerate()
        .filter_map(|(i, f)| f.as_ref().filter(|f| f.multicast).map(|f| (i + 1, f)))
        .map(|(n, f)| {
            json!({"number": n, "name": f.name, "channels": f.channels,
                   "group": f.to.0.to_string(), "port": f.to.1})
        })
        .collect();
    let id = idn_common::hex::hex(&d.identity.device_id);
    let (started, rx_beyond, tx_beyond, name) = kept(|k| {
        let name = match &k.clash {
            Some((wanted, used)) if *used == d.identity.name => wanted.clone(),
            _ => d.identity.name.clone(),
        };
        (
            k.started_latency_ns,
            k.rx_beyond.clone(),
            k.tx_beyond.clone(),
            name,
        )
    });
    let mut rx: Vec<Value> = d.rx.iter().map(|c| json!({
        "name": c.name,
        "from": c.subscription.as_ref().map(|s| json!({"channel": s.channel, "device": s.device})),
    })).collect();
    rx.extend(rx_beyond);
    let mut tx: Vec<Value> = d.tx.iter().map(|c| json!(c.name)).collect();
    tx.extend(tx_beyond);
    json!({
        // Its device ID, kept: a new address (DHCP) isn't a new device to the network's
        // controllers (bug run J5-5; spec §1.1 lets it be set).
        "device_id": id,
        "name": name,
        "rx": rx,
        "tx": tx,
        "rx_latency_ns": d.rx_latency_ns,
        "started_latency_ns": started,
        "multicast": flows,
    })
}

/// Where the IDN source's channels come from (idn-routing.md §A.16): receive channel k is
/// its channel k, `DEVICE/CHANNEL` where subscribed ("" where not).
pub fn origin(d: &Device) -> idn_open_protocol::node::routing::AudioOrigin {
    idn_open_protocol::node::routing::AudioOrigin {
        kind: "network_device".into(),
        device: String::new(),
        channels: d
            .rx
            .iter()
            .map(|c| {
                c.subscription
                    .as_ref()
                    .map_or(String::new(), |s| format!("{}/{}", s.device, s.channel))
            })
            .collect(),
    }
}

/// Saved state onto `d` (what fits: more channels saved than it has are left out).
/// Subscriptions come back unresolved (§7.1).
pub fn apply(d: &mut Device, v: &Value) {
    // Sixteen hex digits (bug run K2-5: 16 bytes with a multi-byte character were cut
    // inside it, a panic at every start).
    if let Some(id) = v["device_id"]
        .as_str()
        .filter(|s| s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        && let Some(bytes) = (0..8)
            .map(|i| u8::from_str_radix(&id[2 * i..2 * i + 2], 16).ok())
            .collect::<Option<Vec<u8>>>()
        && bytes.iter().any(|&b| b != 0)
    {
        d.identity.device_id.copy_from_slice(&bytes);
        d.identity.factory_name = format!("idn-aoip-{}", id.to_ascii_lowercase());
    }
    if let Some(n) = v["name"]
        .as_str()
        .filter(|n| idn_aoip_device::device::device_name_ok(n))
    {
        d.identity.name = n.to_owned();
    }
    for (c, s) in
        d.rx.iter_mut()
            .zip(v["rx"].as_array().into_iter().flatten())
    {
        if let Some(n) = s["name"]
            .as_str()
            .filter(|n| idn_aoip_device::device::channel_name_ok(n))
        {
            c.name = n.to_owned();
        }
        if let (Some(ch), Some(dev)) = (s["from"]["channel"].as_str(), s["from"]["device"].as_str())
        {
            c.subscription = Some(Subscription {
                channel: ch.to_owned(),
                device: dev.to_owned(),
            });
            c.status = status::UNRESOLVED;
        }
    }
    for (c, n) in
        d.tx.iter_mut()
            .zip(v["tx"].as_array().into_iter().flatten())
    {
        if let Some(n) = n
            .as_str()
            .filter(|n| idn_aoip_device::device::channel_name_ok(n))
        {
            c.name = n.to_owned();
        }
    }
    // A controller's latency, while the device runs with the setting it was saved under
    // (N7-1); a file from before that was kept: the setting.
    let started = kept(|k| k.started_latency_ns);
    if let Some(l) = v["rx_latency_ns"].as_u64().map(|l| l as u32)
        && (d.latency_min_ns..=d.latency_max_ns).contains(&l)
        && (started.is_none() || v["started_latency_ns"].as_u64() == started.map(u64::from))
    {
        d.rx_latency_ns = l;
    }
    // Channels saved past the ones it has: kept for the next save (N7-6).
    let beyond = |k: &str, have: usize| -> Vec<Value> {
        v[k].as_array()
            .map(|a| a.iter().skip(have).cloned().collect())
            .unwrap_or_default()
    };
    let (rx_beyond, tx_beyond) = (beyond("rx", d.rx.len()), beyond("tx", d.tx.len()));
    if !rx_beyond.is_empty() || !tx_beyond.is_empty() {
        kept(|k| {
            k.rx_beyond = rx_beyond;
            k.tx_beyond = tx_beyond;
        });
    }
    let bytes = usize::from(d.format.bits / 8);
    for f in v["multicast"].as_array().into_iter().flatten() {
        let (Some(n), Some(g), Some(p)) = (
            f["number"].as_u64(),
            f["group"].as_str().and_then(|g| g.parse::<Ipv4Addr>().ok()),
            f["port"].as_u64(),
        ) else {
            continue;
        };
        let chans: Vec<u16> = f["channels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c.as_u64().map(|c| c as u16))
            .filter(|&c| usize::from(c) <= d.tx.len())
            .collect();
        // A group, a port, at most 8 channels (a corrupt file's port 0 made every packet
        // fail, bug run I2-2).
        // (And a group a controller may ask for: an edited file's 224.0.0.x or port 5353
        // flooded the network or its mDNS; bug review 2.)
        if chans.is_empty()
            || chans.len() > 8
            || p == 0
            || p > 65535
            || !idn_aoip_device::flows::group_ok(g, p as u16)
            || n == 0
            || n > 32
        {
            continue;
        }
        let name = f["name"].as_str().map(str::to_owned);
        d.tx_flows
            .create_multicast(n as u16, name, chans, (g, p as u16), bytes);
    }
}

pub fn load(file: &Path, d: &mut Device) {
    // One that can't be read (bad JSON, or a read error) is said and set aside under a
    // name of its own before a new one is saved over it (bug run K2-5: replaced silently;
    // N3-5: a read error replaced it, and a second bad start overwrote the first's copy).
    match idn_common::fsutil::load_saved(file, |b| serde_json::from_slice::<Value>(b).ok()) {
        Ok(Some(v)) => apply(d, &v),
        Ok(None) => {}
        Err(e) => eprintln!("idn-aoip: {e}"),
    }
}

/// The device this program ran last (per port block) and, when `name` has no state of its
/// own yet, that device's folder moved to `name`'s: the old name, when it was. Writes
/// `name` as the last one.
pub fn follow_rename(dir: &Path, name: &str, ports: Option<u16>) -> Option<String> {
    let last = dir.join(match ports {
        Some(p) => format!("last-device-{p}"),
        None => "last-device".to_owned(),
    });
    let new = dir.join(name);
    let mut moved = None;
    if !new.join("device.json").exists()
        && let Ok(old) = std::fs::read_to_string(&last)
    {
        let old = old.trim().to_owned();
        let from = dir.join(&old);
        if old != name
            && idn_aoip_device::device::device_name_ok(&old)
            && from.join("device.json").exists()
        {
            // (An empty folder of the new name, from a start that failed, out of the way.)
            let _ = std::fs::remove_dir(&new);
            match std::fs::rename(&from, &new) {
                Ok(()) => moved = Some(old),
                Err(e) => eprintln!(
                    "idn-aoip: {} to {}: {e}; started as a new device",
                    from.display(),
                    new.display()
                ),
            }
        }
    }
    let _ = std::fs::create_dir_all(dir);
    let _ = idn_common::fsutil::write_atomic(&last, name.as_bytes());
    moved
}

/// What was loaded or imported, made to keep the device's own rules (bug run M1-9, M2-7):
/// an output name that another output has (any case), or another's default or factory
/// name, back to its default; a subscription to a name no request could make dropped.
/// How many were changed.
pub fn clean(d: &mut Device) -> usize {
    use idn_aoip_device::device::{channel_name_ok, device_name_ok};
    let mut n = 0;
    let count = d.tx.len();
    for i in 0..count {
        let a = d.tx[i].name.clone();
        let clash = (0..count).any(|j| {
            j != i
                && ((j < i && a.eq_ignore_ascii_case(&d.tx[j].name))
                    || a.eq_ignore_ascii_case(&format!("TX {}", j + 1))
                    || a.eq_ignore_ascii_case(&Device::factory_channel_name(j + 1)))
        });
        if clash || !channel_name_ok(&a) {
            d.tx[i].name = format!("TX {}", i + 1);
            n += 1;
        }
    }
    for c in &mut d.rx {
        let bad = c.subscription.as_ref().is_some_and(|s| {
            !channel_name_ok(&s.channel) || !(s.device == "." || device_name_ok(&s.device))
        });
        if bad {
            c.subscription = None;
            c.status = status::NONE;
            n += 1;
        }
    }
    n
}

pub fn save(file: &Path, d: &Device) {
    let text = to_json(d).to_string();
    if let Err(e) = idn_common::fsutil::write_atomic(file, text.as_bytes()) {
        eprintln!("idn-aoip: {}: {e}", file.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use idn_aoip_device::device::Identity;

    /// Renamed in the Node: its folder moved to the new name (bug run N3-2); a name with a
    /// state of its own keeps it; another port block is another device.
    #[test]
    fn a_renamed_device_keeps_its_state() {
        let dir = idn_common::testing::TempDir::new("aoip-rename");
        std::fs::create_dir_all(dir.join("FOH")).unwrap();
        std::fs::write(dir.join("FOH/device.json"), "{}").unwrap();
        assert_eq!(follow_rename(&dir, "FOH", None), None);
        assert_eq!(follow_rename(&dir, "Stage", None).as_deref(), Some("FOH"));
        assert!(dir.join("Stage/device.json").exists() && !dir.join("FOH").exists());
        // Another port block: its own last device.
        assert_eq!(follow_rename(&dir, "Other", Some(2)), None);
        // Back to a name that has its own state again: kept.
        std::fs::create_dir_all(dir.join("B")).unwrap();
        std::fs::write(dir.join("B/device.json"), "{}").unwrap();
        assert_eq!(follow_rename(&dir, "B", None), None);
        assert!(dir.join("Stage/device.json").exists());
    }

    /// The IDN source's origin (§A.16): each receive channel's subscription as
    /// `DEVICE/CHANNEL`, "" where none; and `device.json` keeps them for a desk to read.
    #[test]
    fn the_sources_origin_is_its_subscriptions() {
        let mut d = Device::new(
            Identity::idn("FOH-IDN", Ipv4Addr::new(10, 0, 0, 7), 0),
            3,
            2,
        );
        d.rx[0].subscription = Some(Subscription {
            channel: "Mic 5".into(),
            device: "Stage Box Left".into(),
        });
        d.rx[2].subscription = Some(Subscription {
            channel: "Mic 6".into(),
            device: "Stage Box Left".into(),
        });
        let o = origin(&d);
        assert_eq!(o.kind, "network_device");
        assert_eq!(
            o.channels,
            vec!["Stage Box Left/Mic 5", "", "Stage Box Left/Mic 6"]
        );
        let v = to_json(&d);
        assert_eq!(v["rx"][0]["from"]["device"], "Stage Box Left");
        assert_eq!(v["rx"][0]["from"]["channel"], "Mic 5");
        assert!(v["rx"][1]["from"].is_null());
    }

    #[test]
    fn kept_and_back() {
        let mut d = Device::new(
            Identity::idn("Booth-IDN", Ipv4Addr::new(10, 0, 0, 7), 0),
            2,
            2,
        );
        d.identity.name = "Main-Hall".into();
        d.rx[1].name = "Pastor".into();
        d.rx[1].subscription = Some(Subscription {
            channel: "Mic 1".into(),
            device: "Stage-Box".into(),
        });
        d.tx[0].name = "Main L".into();
        d.rx_latency_ns = 5_000_000;
        d.tx_flows
            .create_multicast(
                3,
                Some("f1".into()),
                vec![1, 2],
                (Ipv4Addr::new(239, 255, 1, 2), 4321),
                3,
            )
            .unwrap();
        let v = to_json(&d);
        // Another address now (DHCP): the ID it had is kept.
        let mut e = Device::new(
            Identity::idn("Booth-IDN", Ipv4Addr::new(10, 0, 0, 99), 0),
            2,
            2,
        );
        apply(&mut e, &v);
        assert_eq!(e.identity.device_id, d.identity.device_id);
        assert_eq!(e.identity.name, "Main-Hall");
        assert_eq!(e.rx[1].name, "Pastor");
        assert_eq!(e.rx[1].subscription, d.rx[1].subscription);
        assert_eq!(e.rx[1].status, status::UNRESOLVED);
        assert_eq!(e.tx[0].name, "Main L");
        assert_eq!(e.rx_latency_ns, 5_000_000);
        let f = e.tx_flows.flows[2].as_ref().unwrap();
        assert_eq!(
            (f.name.as_deref(), f.channels.clone()),
            (Some("f1"), vec![1, 2])
        );
    }
}
