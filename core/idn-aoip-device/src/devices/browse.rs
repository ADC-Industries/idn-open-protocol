//! Finding the devices: mDNS asked (with known answers, backing off) and heard, each device's place kept.

use super::*;

/// A device's PTR heard this recently isn't resolved again.
pub(crate) const BROWSE_EVERY: Duration = Duration::from_secs(10);
/// The asking backs off from 1 s, doubling, to an hour (RFC 6762 §5.2; bug run K3-6: the
/// same query every 10 s with no known answers, every device's whole channel set back).
pub(crate) const ASK_FIRST: Duration = Duration::from_secs(1);
// Two minutes at most (bug run M3-2: an hour, and a device that only answers queries,
// never announcing, stayed unlisted that long); known answers keep it cheap.
pub(crate) const ASK_MOST: Duration = Duration::from_secs(120);
/// A device read this recently is a known answer; older, it answers queries again (its
/// mDNS keeping it listed while the reader is slow: bug run M3-1).
pub(crate) const KNOWN_FOR: Duration = Duration::from_secs(45);
/// A device that answered reads is kept this long after its last answer (three re-reads
/// missed), whatever mDNS says.
pub(crate) const ANSWERED_KEEPS: Duration = Duration::from_secs(90);
/// Channels asked for (only while a device refuses the channel reads, lab lt18).
pub(crate) const CHANNELS_EVERY: Duration = Duration::from_secs(30);

/// Asks for `_netaudio-arc._udp` every 10 s and takes every answer heard on the mDNS port
/// (its own and others' asking): a device keeps its place while it answers; a goodbye
/// (TTL 0) drops it at once.
pub(crate) fn browse(
    ip: Ipv4Addr,
    devices: &Mutex<BTreeMap<String, Entry>>,
    heard: Option<mdns::Heard>,
    dropped: &std::sync::atomic::AtomicBool,
) {
    // The responses the program's responder hears (one socket on 5353, bug run I3-12);
    // without one, a listener of its own.
    let (socket, rx) = match heard {
        Some(h) => (h.socket, Some(h.rx)),
        None => match mdns::listener(ip) {
            Ok(s) => (std::sync::Arc::new(s), None),
            Err(e) => {
                idn_common::log::log_or_stderr(&format!(
                    "idn-aoip: network devices: can't look for them ({e})"
                ));
                return;
            }
        },
    };
    let kind = mdns::type_name("_netaudio-arc");
    let chan = mdns::type_name("_netaudio-chan");
    let mut next = Instant::now();
    let mut every = ASK_FIRST;
    let mut chan_next = Instant::now() + CHANNELS_EVERY;
    let mut failed_resolves: std::collections::HashMap<String, Instant> = Default::default();
    let mut buf = vec![0u8; 9000];
    loop {
        let now = Instant::now();
        if dropped.swap(false, std::sync::atomic::Ordering::Relaxed) {
            every = ASK_FIRST;
            next = next.min(now + ASK_FIRST);
        }
        if now >= next {
            // The devices that answer reads given as known answers: only new ones (and
            // changed ones, by their own announcements) answer.
            let known: Vec<dns::Record> = devices
                .lock_ok()
                .iter()
                .filter(|(_, e)| {
                    e.answered_at
                        .is_some_and(|t| now.duration_since(t) < KNOWN_FOR)
                })
                .map(|(n, _)| {
                    let mut inst = vec![n.clone()];
                    inst.extend(kind.iter().cloned());
                    dns::Record {
                        name: kind.clone(),
                        rtype: dns::TYPE_PTR,
                        class: dns::CLASS_IN,
                        ttl: 4500,
                        data: dns::RData::Ptr(inst),
                    }
                })
                .take(64)
                .collect();
            mdns::send(
                &socket,
                &mdns::query_with_known(&[(kind.clone(), dns::TYPE_PTR)], &known),
                None,
            );
            next = now + every;
            // Under a minute while a listed device doesn't answer reads (it stays listed by
            // mDNS alone; bug run L3-5: it dropped out every ~2 min).
            let unread = devices.lock_ok().values().any(|e| {
                e.seen.is_some()
                    && e.answered_at
                        .is_none_or(|t| now.duration_since(t) >= UNSEEN)
            });
            every = (every * 2).min(if unread { UNSEEN / 2 } else { ASK_MOST });
        }
        // The channels, only while a device refuses its channel reads (announced ones
        // are all that is listed of it).
        if now >= chan_next {
            chan_next = now + CHANNELS_EVERY;
            let refusing = devices
                .lock_ok()
                .values()
                // Answering its name but not its channel reads (not one that doesn't
                // answer at all: M3-3, every device's whole channel set each 30 s).
                .any(|e| e.error.is_some() && e.answered_at.is_some_and(|t| t.elapsed() < UNSEEN));
            if refusing {
                mdns::send(
                    &socket,
                    &dns::encode_query(0, &[(chan.clone(), dns::TYPE_PTR)], false),
                    None,
                );
            }
        }
        let (m, from) = match &rx {
            Some(rx) => match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(x) => x,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => return,
            },
            None => {
                let Ok((n, from)) = socket.recv_from(&mut buf) else {
                    continue;
                };
                let (Some(m), IpAddr::V4(from)) = (dns::parse(&buf[..n]), from.ip()) else {
                    continue;
                };
                (m, from)
            }
        };
        if !m.is_response() {
            continue;
        }
        channels_announced(&m, &chan, from, devices);
        let mut found: Vec<(String, Seen)> = Vec::new();
        let mut ask_for: Vec<(String, String)> = Vec::new();
        for (name, seen) in announced(&m, &kind, from) {
            match seen {
                Some(Announced::Gone) => {
                    devices.lock_ok().remove(&name);
                    // A rename's new name looked for at once (M3-2: its announcements may
                    // be lost, and the asking backed off).
                    dropped.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                Some(Announced::At(s)) => found.push((name, s)),
                // Its PTR alone: its SRV and TXT asked for, all of the packet's together
                // after it, at most 32, and not one seen in the last 10 s (bug run I3-5:
                // 600 ms each, in turn, stalled the browse for minutes).
                None => {
                    // One that failed to resolve in the last 10 s isn't asked again (K3-8:
                    // each packet naming it held the browse 600 ms).
                    if failed_resolves
                        .get(&name)
                        .is_some_and(|t: &Instant| t.elapsed() < BROWSE_EVERY)
                    {
                        continue;
                    }
                    let recent = devices
                        .lock_ok()
                        .get(&name)
                        .and_then(|e| e.last_seen)
                        .is_some_and(|t| t.elapsed() < BROWSE_EVERY);
                    if recent {
                        let mut d = devices.lock_ok();
                        if let Some(e) = d.get_mut(&name) {
                            e.last_seen = Some(Instant::now());
                        }
                    } else if ask_for.len() < 32 {
                        ask_for.push((name, "_netaudio-arc".into()));
                    }
                }
            }
        }
        if !ask_for.is_empty() {
            let answers = mdns::resolve_many(ip, &ask_for, Duration::from_millis(600));
            failed_resolves.retain(|_, t| t.elapsed() < BROWSE_EVERY);
            for ((name, _), r) in ask_for.into_iter().zip(answers) {
                match r {
                    Some(r) => found.push((name, seen_of(r.addr, &r.txt))),
                    None => {
                        if failed_resolves.len() < 1024 {
                            failed_resolves.insert(name, Instant::now());
                        }
                    }
                }
            }
        }
        let mut d = devices.lock_ok();
        for (name, seen) in found {
            // At most MOST_DEVICES listed; a new one past that waits for room.
            if d.len() >= MOST_DEVICES && !d.contains_key(&name) {
                continue;
            }
            let e = d.entry(name).or_default();
            // A TXT-less answer keeps what an earlier one said (bug run I3-9: its protocol
            // version was lost and a modern device read the legacy way).
            let old = e.seen.clone();
            let keep = |v: Option<String>, f: fn(&Seen) -> Option<String>| {
                v.or_else(|| old.as_ref().and_then(f))
            };
            let seen = Seen {
                arc: seen.arc,
                maker: keep(seen.maker, |s| s.maker.clone()),
                model: keep(seen.model, |s| s.model.clone()),
                arc_version: keep(seen.arc_version, |s| s.arc_version.clone()),
            };
            if old.as_ref() != Some(&seen) {
                // Changed: read again soon, within the 1 s limit (I3-9: a device answering
                // two ways was read at every packet); moved: at once.
                e.wanted = true;
                if old.as_ref().is_some_and(|s| s.arc != seen.arc) {
                    e.read_at = None;
                }
            }
            e.seen = Some(seen);
            e.last_seen = Some(Instant::now());
        }
    }
}

/// The most devices listed (a network's real ones are far fewer; a flood stops here).
pub(crate) const MOST_DEVICES: usize = 1024;

/// The transmit channels a response announces, kept with their devices (a device not
/// listed yet is left out; a goodbye drops the channel).
pub(crate) fn channels_announced(
    m: &dns::Message,
    chan: &dns::Name,
    from: Ipv4Addr,
    devices: &Mutex<BTreeMap<String, Entry>>,
) {
    let mut d = devices.lock_ok();
    for r in &m.records {
        let RData::Ptr(instance) = &r.data else {
            continue;
        };
        if !dns::name_eq(&r.name, chan) {
            continue;
        }
        let Some((channel, device)) = instance.first().and_then(|n| n.rsplit_once('@')) else {
            continue;
        };
        let Some(e) = d.get_mut(device) else { continue };
        if r.ttl == 0 {
            e.announced_tx.remove(channel);
            continue;
        }
        let res = mdns::resolved_from(m, instance, from);
        let id = res.as_ref().and_then(|res| {
            res.txt
                .iter()
                .find_map(|t| t.strip_prefix("id=").and_then(|v| v.trim().parse().ok()))
        });
        let factory = res.is_some_and(|r| r.txt.iter().any(|t| t == "default"));
        // Old ones out before the cap is reached (K3-8: never pruned, a device past 512
        // stale names listed no new channel).
        if e.announced_tx.len() >= MOST_ANNOUNCED {
            e.announced_tx.retain(|_, (_, t, _)| t.elapsed() < UNSEEN);
        }
        if e.announced_tx.len() < MOST_ANNOUNCED || e.announced_tx.contains_key(channel) {
            let (had, was) = e
                .announced_tx
                .get(channel)
                .map_or((None, false), |(i, _, f)| (*i, *f));
            e.announced_tx.insert(
                channel.to_owned(),
                (id.or(had), Instant::now(), factory || was),
            );
        }
    }
}

/// What an answer says of an instance.
pub(crate) enum Announced {
    At(Seen),
    /// Its PTR with TTL 0: gone.
    Gone,
}

pub(crate) fn seen_of(arc: SocketAddr, txt: &[String]) -> Seen {
    let get = |k: &str| {
        txt.iter()
            .find_map(|t| t.strip_prefix(k).and_then(|v| v.strip_prefix('=')))
            .map(str::to_owned)
    };
    Seen {
        arc,
        maker: get("mf"),
        model: get("model"),
        arc_version: get("arcp_vers"),
    }
}

/// The devices one response names: instance → where its ARC port is (`None`: only its
/// PTR came, its SRV to be asked for).
pub(crate) fn announced(
    m: &dns::Message,
    kind: &dns::Name,
    from: Ipv4Addr,
) -> Vec<(String, Option<Announced>)> {
    let mut out = Vec::new();
    for r in &m.records {
        let RData::Ptr(instance) = &r.data else {
            continue;
        };
        if !dns::name_eq(&r.name, kind) {
            continue;
        }
        let Some(name) = instance.first().cloned() else {
            continue;
        };
        if r.ttl == 0 {
            out.push((name, Some(Announced::Gone)));
            continue;
        }
        let at = mdns::resolved_from(m, instance, from)
            .map(|res| Announced::At(seen_of(res.addr, &res.txt)));
        out.push((name, at));
    }
    out
}
