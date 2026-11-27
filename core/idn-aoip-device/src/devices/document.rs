//! The list as the Node passes it on (devices.xml).

use super::*;

/// The list as the Node passes it on: `<network_devices>` with a `<device>` each.
pub(crate) fn document(
    (own, own_arc): (&str, Option<SocketAddr>),
    devices: &BTreeMap<String, Entry>,
) -> String {
    let mut root = Element::new("network_devices").with_attr("by", own);
    for (instance, e) in devices {
        let Some(seen) = &e.seen else { continue };
        let mut d = Element::new("device")
            .with_attr("instance", instance)
            .with_attr("address", &seen.arc.ip().to_string())
            .with_attr("arc_port", &seen.arc.port().to_string());
        // By its control address when known (bug run L3-3: started as "X-2" beside a device
        // named X, X was marked this one's).
        if own_arc.map_or(instance == own, |a| a == seen.arc) {
            d = d.with_attr("this", "true");
        }
        // Its settings (R.32 stage 2), as read.
        if let Some(st) = &e.settings {
            let ns = |v: Option<u32>| v.map(|n| format!("{:.3}", f64::from(n) / 1e6));
            for (k, v) in [
                ("rate", st.rate.map(|r| r.to_string())),
                ("latency_ms", ns(st.latency_ns)),
                ("active_latency_ms", ns(st.active_latency_ns)),
                ("latency_min_ms", ns(st.latency_min_ns)),
                ("latency_max_ms", ns(st.latency_max_ns)),
                ("aes67", st.aes67.map(|a| a.to_string())),
            ] {
                if let Some(v) = v {
                    d = d.with_attr(k, &v);
                }
            }
        }
        for (k, v) in [
            ("maker", &seen.maker),
            ("model", &seen.model),
            ("arc_version", &seen.arc_version),
        ] {
            if let Some(v) = v {
                d = d.with_attr(k, v);
            }
        }
        if let Some(why) = &e.error {
            d = d.with_attr("error", why);
        }
        // Nothing read: its name is its mDNS instance's (a device refusing every read
        // still shows, by name, with its announced channels).
        if e.reading.is_none() {
            d = d.with_attr("name", instance);
        }
        if let Some(r) = &e.reading {
            d = d
                .with_attr("name", &r.name)
                .with_attr("tx_count", &r.counts.tx.to_string())
                .with_attr("rx_count", &r.counts.rx.to_string());
            if r.rx.is_none() && e.error.is_none() {
                d = d.with_attr("channels", "not read (a newer device: stage 1b)");
            }
            for c in r.rx.iter().flatten() {
                let mut x = Element::new("rx")
                    .with_attr("n", &c.number.to_string())
                    .with_attr("name", &c.name)
                    .with_attr("status", &c.subscription_status.to_string());
                if let Some(v) = &c.source_channel {
                    x = x.with_attr("from_channel", v);
                }
                if let Some(v) = &c.source_device {
                    x = x.with_attr("from_device", v);
                }
                d = d.with_child(x);
            }
            for c in r.tx.iter().flatten() {
                let mut x = Element::new("tx").with_attr("n", &c.number.to_string());
                if let Some(v) = &c.name {
                    x = x.with_attr("name", v);
                }
                if let Some(v) = c.sample_rate {
                    x = x.with_attr("rate", &v.to_string());
                }
                d = d.with_child(x);
            }
        }
        // Its transmit channels from its announcements, where its list wasn't read (its
        // reads refused or failed): by their ID, else by name, marked as announced.
        let read_tx = e
            .reading
            .as_ref()
            .and_then(|r| r.tx.as_ref())
            .is_some_and(|t| !t.is_empty());
        if !read_tx && !e.announced_tx.is_empty() {
            let now = Instant::now();
            // One a channel: a device announces a renamed channel twice (its factory name,
            // `default`, and the name it was given), with one ID; the given name kept (bug
            // run J3-4). Channels without an ID after the highest.
            let mut by_id: BTreeMap<u16, (&String, bool)> = BTreeMap::new();
            let mut no_id: Vec<&String> = Vec::new();
            for (name, (id, t, factory)) in &e.announced_tx {
                if now.duration_since(*t) >= UNSEEN {
                    continue;
                }
                match id {
                    Some(i) => {
                        let keep = by_id.get(i).is_none_or(|(_, f)| *f && !factory);
                        if keep {
                            by_id.insert(*i, (name, *factory));
                        }
                    }
                    None => no_id.push(name),
                }
            }
            if !by_id.is_empty() || !no_id.is_empty() {
                d = d.with_attr("tx_from", "announced");
            }
            // Names without an ID only where none had one: beside channels with IDs they
            // are mostly those channels' other names, heard without their TXT (bug run
            // K3-8: both names listed, under made-up numbers).
            if !by_id.is_empty() {
                no_id.clear();
            }
            let next = by_id.keys().max().map_or(1, |m| usize::from(*m) + 1);
            let mut chans: Vec<(usize, &String)> = by_id
                .iter()
                .map(|(i, (n, _))| (usize::from(*i), *n))
                .collect();
            chans.extend(no_id.into_iter().enumerate().map(|(k, n)| (next + k, n)));
            for (n, name) in chans {
                d = d.with_child(
                    Element::new("tx")
                        .with_attr("n", &n.to_string())
                        .with_attr("name", name),
                );
            }
        }
        root = root.with_child(d);
    }
    root.to_xml()
}
