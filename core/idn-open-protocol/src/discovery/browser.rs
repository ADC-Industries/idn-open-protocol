//! The live source list ([`Browser`]), its change events, and one-off searches.

use super::dns::{RData, TYPE_PTR, name, name_eq};
use super::mdns::{MdnsQuerier, preferred_interface};
use super::*;
use idn_common::backoff::Backoff;
use idn_common::sync::LockOk as _;
use std::collections::BTreeMap;
use std::net::SocketAddrV4;
use std::sync::Mutex;
use std::time::Instant;

/// A change in the set of visible sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceEvent {
    /// A source appeared.
    Added(Source),
    /// A known source (same name) changed address, port, groups or features.
    Changed(Source),
    /// A source went away. Sources that stop cleanly disappear at once; sources that
    /// vanish without notice (power loss) after about 20 seconds.
    Removed(Source),
}

/// What a queued change event says about its item.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Change {
    Added,
    Changed,
    Removed,
}

/// Change events ([`SourceEvent`], [`DeviceEvent`]) as queued for an application.
pub(crate) trait ChangeEvent: Sized {
    type Key: PartialEq;
    fn key(&self) -> Self::Key;
    fn change(&self) -> Change;
    /// The same item, reported as `c`.
    fn into_change(self, c: Change) -> Self;
}

/// Changes queued at most, as a last resort (merging keeps the queue as small as the
/// list itself).
const MAX_QUEUED_CHANGES: usize = 4096;

/// Queues `e`, merged with an unread event for the same item: the queue never holds two
/// for one item, so it doesn't grow while the application isn't reading (a UI that
/// polls [`Browser::sources`] instead of calling `recv`). The merged event says what
/// the application needs to catch up: added then changed is added (with the new
/// values), added then removed is nothing, removed then added is changed.
pub(crate) fn queue_change<E: ChangeEvent>(q: &mut std::collections::VecDeque<E>, e: E) {
    let k = e.key();
    let merged = match q.iter().position(|x| x.key() == k) {
        None => Some(e),
        Some(i) => {
            let old = q.remove(i).expect("found above").change();
            match (old, e.change()) {
                (Change::Added, Change::Removed) => None,
                (Change::Added, _) => Some(e.into_change(Change::Added)),
                (_, Change::Removed) => Some(e),
                _ => Some(e.into_change(Change::Changed)),
            }
        }
    };
    q.extend(merged);
    while q.len() > MAX_QUEUED_CHANGES {
        q.pop_front();
    }
}

impl ChangeEvent for SourceEvent {
    type Key = String;
    fn key(&self) -> String {
        let (SourceEvent::Added(s) | SourceEvent::Changed(s) | SourceEvent::Removed(s)) = self;
        s.name.to_lowercase()
    }
    fn change(&self) -> Change {
        match self {
            SourceEvent::Added(_) => Change::Added,
            SourceEvent::Changed(_) => Change::Changed,
            SourceEvent::Removed(_) => Change::Removed,
        }
    }
    fn into_change(self, c: Change) -> Self {
        let (SourceEvent::Added(s) | SourceEvent::Changed(s) | SourceEvent::Removed(s)) = self;
        match c {
            Change::Added => SourceEvent::Added(s),
            Change::Changed => SourceEvent::Changed(s),
            Change::Removed => SourceEvent::Removed(s),
        }
    }
}

/// Settings for [`Browser::start`]. The default searches all interfaces with mDNS.
#[derive(Debug, Clone, Default)]
pub struct BrowserConfig {
    /// The IPv4 address of the network interface to search on. `None`: all interfaces.
    pub interface: Option<Ipv4Addr>,
    /// Use this Discovery Server (`host` or `host:port`) instead of mDNS.
    pub discovery_server: Option<String>,
}

/// A live source list for applications. It keeps a current list ([`sources`]) and
/// reports changes ([`recv`]) from a background thread until dropped.
///
/// [`sources`]: Browser::sources
/// [`recv`]: Browser::recv
pub struct Browser {
    list: Arc<Mutex<BTreeMap<String, Source>>>,
    events: Arc<(
        Mutex<std::collections::VecDeque<SourceEvent>>,
        std::sync::Condvar,
    )>,
    /// Its thread, stopped and waited for with it.
    _worker: idn_common::worker::Worker,
}

/// The browser for `cfg` this process shares: one per interface and Discovery Server,
/// started by the first that asks and ended with the last that holds it. The switcher, the
/// mixer, the routers, the listening page and the site link each started their own,
/// browsing the same network several times over.
pub fn shared(cfg: BrowserConfig) -> io::Result<Arc<Browser>> {
    type Key = (Option<Ipv4Addr>, Option<String>);
    static SHARED: Mutex<BTreeMap<Key, std::sync::Weak<Browser>>> = Mutex::new(BTreeMap::new());
    let key = (cfg.interface, cfg.discovery_server.clone());
    if let Some(b) = SHARED
        .lock_ok()
        .get(&key)
        .and_then(std::sync::Weak::upgrade)
    {
        return Ok(b);
    }
    // Started outside the lock: a Discovery Server's name looked up slowly held up every
    // other browser's start. Two that start at once: the first kept.
    let b = Arc::new(Browser::start(cfg)?);
    let mut all = SHARED.lock_ok();
    if let Some(first) = all.get(&key).and_then(std::sync::Weak::upgrade) {
        return Ok(first);
    }
    all.retain(|_, w| w.strong_count() > 0);
    all.insert(key, Arc::downgrade(&b));
    Ok(b)
}

/// How often a [`Browser`] on automatic interfaces looks for new ones.
const IFACE_SCAN: Duration = Duration::from_secs(5);

/// How long an mDNS source may go unseen before it is considered gone.
const EXPIRY: Duration = Duration::from_secs(20);

impl Browser {
    /// Starts searching. Fails if no mDNS socket can be opened, or the Discovery Server
    /// address doesn't resolve. (An unreachable server is retried in the background.)
    pub fn start(cfg: BrowserConfig) -> io::Result<Self> {
        use std::sync::atomic::Ordering;
        let list: Arc<Mutex<BTreeMap<String, Source>>> = Default::default();
        let events: Arc<(
            Mutex<std::collections::VecDeque<SourceEvent>>,
            std::sync::Condvar,
        )> = Default::default();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Publishes the difference between the current and the previous list.
        let publish = {
            let (list, events) = (list.clone(), events.clone());
            move |now: Vec<Source>| {
                let mut l = list.lock_ok();
                let mut ev = Vec::new();
                let new: BTreeMap<String, Source> = now
                    .into_iter()
                    .map(|s| (s.name.to_lowercase(), s))
                    .collect();
                for (k, s) in &new {
                    match l.get(k) {
                        None => ev.push(SourceEvent::Added(s.clone())),
                        Some(old) if old != s => ev.push(SourceEvent::Changed(s.clone())),
                        _ => {}
                    }
                }
                for (k, s) in l.iter() {
                    if !new.contains_key(k) {
                        ev.push(SourceEvent::Removed(s.clone()));
                    }
                }
                *l = new;
                if !ev.is_empty() {
                    let mut q = events.0.lock_ok();
                    for e in ev {
                        queue_change(&mut q, e);
                    }
                    events.1.notify_all();
                }
            }
        };
        let thread = match cfg.discovery_server {
            Some(sv) => {
                let addr = ds::server_addr(&sv)?;
                let stop2 = stop.clone();
                std::thread::spawn(move || {
                    let mut current: BTreeMap<String, Source> = BTreeMap::new();
                    while !stop2.load(Ordering::Relaxed) {
                        // Short waits that notice `stop`: dropping the browser doesn't hang
                        // while the server is unreachable (R1-101).
                        let Ok(mut w) =
                            ds::SourceWatcher::connect_within(addr, Duration::from_secs(1))
                        else {
                            ds::sleep_or_stop(&stop2, Duration::from_secs(2));
                            continue;
                        };
                        let _ = w.set_timeout(Some(Duration::from_millis(300)));
                        loop {
                            if stop2.load(Ordering::Relaxed) {
                                return;
                            }
                            match w.next_change() {
                                Ok(Some(ds::SourceChange::All(v))) => {
                                    current =
                                        v.into_iter().map(|s| (s.name.to_lowercase(), s)).collect()
                                }
                                Ok(Some(ds::SourceChange::Added(s))) => {
                                    current.insert(s.name.to_lowercase(), s);
                                }
                                Ok(Some(ds::SourceChange::Removed(s))) => {
                                    // Only that registration: a replacement under the same
                                    // name, registered first, stays (R1-94).
                                    let k = s.name.to_lowercase();
                                    if current.get(&k).is_some_and(|c| {
                                        s.addr.is_none() || (c.addr, c.port) == (s.addr, s.port)
                                    }) {
                                        current.remove(&k);
                                    }
                                }
                                Ok(None) => break, // server closed: reconnect
                                Err(e) if crate::transport::is_poll_timeout(&e) => {
                                    continue;
                                }
                                Err(_) => break,
                            }
                            publish(current.values().cloned().collect());
                        }
                        ds::sleep_or_stop(&stop2, Duration::from_secs(1));
                    }
                })
            }
            None => {
                let mut q = MdnsQuerier::open(&BrowseOptions {
                    interface: cfg.interface,
                })?;
                let stop2 = stop.clone();
                let iface = cfg.interface;
                std::thread::spawn(move || {
                    let mut cache = Cache::preferring(iface);
                    let start = Instant::now();
                    let mut next_query = start;
                    // At start 1 s apart, doubling (RFC 6762 §5.2), then every 5 s: a
                    // deliberate exception to the doubling, which is how a source that
                    // lost power (no goodbye) leaves the list within EXPIRY.
                    let mut pace = Backoff::new(Duration::from_secs(1), Duration::from_secs(5));
                    let mut next_scan = start + IFACE_SCAN;
                    while !stop2.load(Ordering::Relaxed) {
                        let now = Instant::now();
                        if now >= next_scan {
                            next_scan = now + IFACE_SCAN;
                            if q.refresh() {
                                // A new network: query it at the start-up pace.
                                next_query = now;
                                pace.reset();
                            }
                        }
                        if now >= next_query {
                            let _ = q.query(&cache);
                            next_query = now + pace.next();
                        }
                        let got = q.drain(&mut cache);
                        // The list is rebuilt only when something came or went (R1-100).
                        if cache.expire(EXPIRY) | got {
                            publish(
                                cache
                                    .sources()
                                    .into_iter()
                                    .filter(|s| s.addr.is_some())
                                    .collect(),
                            );
                        }
                        if !got {
                            std::thread::sleep(Duration::from_millis(20));
                        }
                    }
                })
            }
        };
        Ok(Browser {
            list,
            events,
            _worker: idn_common::worker::Worker::from_parts(stop, thread),
        })
    }

    /// The sources currently visible, in no particular order.
    pub fn sources(&self) -> Vec<Source> {
        self.list.lock_ok().values().cloned().collect()
    }

    /// The next change, waiting up to `timeout`; `None` if nothing changed meanwhile.
    /// Changes queue up until read, so a UI can poll this from its own loop; unread
    /// changes to the same source are merged into one.
    pub fn recv(&self, timeout: Duration) -> Option<SourceEvent> {
        let (q, cv) = &*self.events;
        let mut q = q.lock_ok();
        if q.is_empty() {
            q = cv
                .wait_timeout(q, timeout)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        q.pop_front()
    }
}

/// Searches the local network (mDNS) for `timeout` and returns every source found.
/// For a UI, prefer [`Browser`], which keeps the list current.
pub fn browse(timeout: Duration, opts: &BrowseOptions) -> io::Result<Vec<Source>> {
    browse_until(timeout, opts, |_| false)
}

/// Searches for one source by full name (case-insensitive) until its address is known
/// or `timeout` passes. `Ok(None)`: not found in time.
pub fn find(
    full_name: &str,
    timeout: Duration,
    opts: &BrowseOptions,
) -> io::Result<Option<Source>> {
    let pick = |v: &[Source]| {
        v.iter()
            .find(|s| s.name.eq_ignore_ascii_case(full_name) && s.addr.is_some())
            .cloned()
    };
    // A host on several networks has an address on each: wait a moment for the one on
    // the network searched (the answer to our own query), else take what is known.
    let prefer = preferred_interface(opts.interface);
    let mut found_at: Option<Instant> = None;
    let all = browse_until(timeout, opts, |v| {
        let Some(s) = pick(v) else { return false };
        let on_ours = matches!((s.addr, &prefer), (Some(IpAddr::V4(a)), Some(p)) if p.contains(a));
        on_ours
            || prefer.is_none()
            || found_at.get_or_insert_with(Instant::now).elapsed() >= Duration::from_millis(500)
    })?;
    Ok(pick(&all))
}

/// DNS-SD service-type enumeration (RFC 6763 §9): the service types advertised on the
/// local link, e.g. `_airplay._tcp.local`. Used by the setup checker to spot networks
/// shared with phones and consumer devices.
#[doc(hidden)]
pub fn service_types(timeout: Duration, opts: &BrowseOptions) -> io::Result<Vec<String>> {
    let q = MdnsQuerier::open(opts)?;
    let enum_name = name("_services._dns-sd._udp.local");
    let pkt = dns::encode_query(0, &[(enum_name.clone(), TYPE_PTR)], false);
    let dest = SocketAddr::V4(SocketAddrV4::new(MDNS_GROUP, MDNS_PORT));
    let mut types = std::collections::BTreeSet::new();
    let start = Instant::now();
    let mut next = start;
    let mut buf = [0u8; 9000];
    while start.elapsed() < timeout {
        if Instant::now() >= next {
            // From the ephemeral sockets (unicast replies) AND from port 5353 on every
            // interface (multicast replies): a firewall that only admits UDP 5353 would
            // otherwise drop every answer.
            for (_, s) in &q.oneshot {
                let _ = s.send_to(&pkt, dest);
            }
            if let Some(s) = &q.shared {
                for i in &q.ifaces {
                    if socket2::SockRef::from(s).set_multicast_if_v4(i).is_ok() {
                        let _ = s.send_to(&pkt, dest);
                    }
                }
            }
            next = Instant::now() + Duration::from_millis(700);
        }
        let mut got = false;
        for s in q.shared.iter().chain(q.oneshot.iter().map(|(_, s)| s)) {
            // At most 256 per socket per call: a flood (a switching loop replicating
            // multicast) must not starve queries, expiry and the stop check (R1-97).
            for _ in 0..256 {
                let Ok((n, _)) = s.recv_from(&mut buf) else {
                    break;
                };
                got = true;
                if let Some(m) = dns::parse(&buf[..n]).filter(|m| m.is_response()) {
                    for r in &m.records {
                        if let RData::Ptr(t) = &r.data
                            && name_eq(&r.name, &enum_name)
                        {
                            types.insert(t.join(".").to_lowercase());
                        }
                    }
                }
            }
        }
        if !got {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    Ok(types.into_iter().collect())
}
