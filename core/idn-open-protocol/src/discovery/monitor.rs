//! Site-wide monitoring through a Discovery Server (§8.3.6, §E.5.1): every source and
//! receiver that advertises there, with its live properties.

use super::ds::{self, Role, Session, Subject};
use crate::xml::Element;
use idn_common::sync::LockOk as _;
use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Whether a monitored device sends or receives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DeviceKind {
    /// A source (an output of some application, a camera…).
    Source,
    /// A receiver (an input of some application, a monitor…).
    Receiver,
}

/// A device on the monitoring channel and its latest reported values.
///
/// [`properties`](Device::properties) holds everything the device reports, by name.
/// NDI devices report the standard names (e.g. `connection-state`, `source-name`,
/// `video-resolution`). IDN devices add live counters whose names start with `x-idn-`,
/// most usefully read through the accessors below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// Source or receiver.
    pub kind: DeviceKind,
    /// The device's id on the Discovery Server (stable while it stays connected).
    pub id: String,
    /// Full name, `HOST (name)`.
    pub name: String,
    /// The device's address as the Discovery Server saw it.
    pub address: Option<IpAddr>,
    /// Latest value of every reported property, by name. Values are text, as sent.
    pub properties: BTreeMap<String, String>,
    /// For sources without `x-idn-fps` (NDI): the last `video-frames-sent` sample, and
    /// the frame rate estimated from it, in thousandths.
    sent_sample: Option<(u64, Instant)>,
    est_fps_milli: Option<u64>,
}

impl Device {
    /// A property's latest value.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.properties.get(name).map(String::as_str)
    }

    /// The device is an IDN device (it reports IDN properties).
    pub fn is_idn(&self) -> bool {
        self.properties.keys().any(|k| k.starts_with("x-idn-"))
    }

    /// `connection-state` is `connected`: a receiver receiving, or a source with at least
    /// one receiver.
    pub fn connected(&self) -> bool {
        self.get("connection-state") == Some("connected")
    }

    /// For a receiver, the source it is receiving (`source-name`), if any.
    pub fn receiving_from(&self) -> Option<&str> {
        self.get("source-name").filter(|s| !s.is_empty())
    }

    /// Video frames per second: received (receivers) or sent (sources). IDN devices
    /// report it directly. For NDI sources it is estimated from their frames-sent counter,
    /// which they report every 5 s, so it appears about 10 s after subscribing. NDI
    /// receivers report nothing to estimate from.
    pub fn fps(&self) -> Option<f64> {
        match self.get("x-idn-fps") {
            Some(v) => v.parse().ok(),
            None => self.est_fps_milli.map(|m| m as f64 / 1000.0),
        }
    }

    /// Whether the session is encrypted: for a receiver, its current session; for a
    /// source, whether any receiver is encrypted. IDN devices only.
    pub fn encrypted(&self) -> Option<bool> {
        match self.kind {
            DeviceKind::Receiver => self.get("x-idn-encrypted").map(|v| v == "true"),
            DeviceKind::Source => self
                .get("x-idn-receivers-encrypted")?
                .parse::<u64>()
                .ok()
                .map(|n| n > 0),
        }
    }

    /// A receiver's frames dropped because the application fell behind, plus frames lost
    /// on the network. IDN receivers only.
    pub fn frames_dropped(&self) -> Option<u64> {
        let d: u64 = self.get("x-idn-frames-dropped")?.parse().ok()?;
        let l: u64 = self
            .get("x-idn-frames-lost")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        // Device-reported values: never trusted not to overflow.
        Some(d.saturating_add(l))
    }

    /// A receiver's network round-trip time to its source, in milliseconds. IDN only.
    pub fn rtt_ms(&self) -> Option<f64> {
        self.get("x-idn-rtt-ms")?.parse().ok()
    }
}

/// A change on the monitoring channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceEvent {
    /// A device started advertising.
    Added(Device),
    /// A device reported new values.
    Changed(Device),
    /// A device went away (or the Discovery Server connection was lost).
    Removed(Device),
}

type Table = Arc<Mutex<BTreeMap<(DeviceKind, String), Device>>>;
type Events = Arc<(Mutex<VecDeque<DeviceEvent>>, Condvar)>;

/// How often the full list is fetched again: a safety net, since servers push new
/// advertisers (§8.3.9) and removals (§8.3.8) as they happen.
const RELIST: Duration = Duration::from_secs(10);

/// A live table of every device advertising on a Discovery Server, with their reported
/// values: frames per second, drops, encryption, what each receiver is watching.
///
/// Devices appear here only if they advertise themselves. IDN outputs and inputs do so
/// automatically when they use a Discovery Server. NDI devices do only if their
/// application enables it, and NDI receivers report no counters. Keeps running (and
/// reconnecting) until dropped. While the server is unreachable the last known list
/// stays, as in NDI apps; [`server_reachable`](Monitor::server_reachable) tells the UI.
///
/// ```no_run
/// use idn_open_protocol::discovery::{DeviceEvent, Monitor};
/// use std::time::Duration;
///
/// # fn main() -> std::io::Result<()> {
/// let monitor = Monitor::start("192.168.1.10")?;
/// std::thread::sleep(Duration::from_secs(2));
/// for d in monitor.devices() {
///     println!("{:?} {}: {:?} fps, encrypted {:?}", d.kind, d.name, d.fps(), d.encrypted());
/// }
/// while let Some(ev) = monitor.recv(Duration::from_secs(1)) {
///     if let DeviceEvent::Removed(d) = ev {
///         println!("{} went away", d.name);
///     }
/// }
/// # Ok(())
/// # }
/// ```
pub struct Monitor {
    table: Table,
    events: Events,
    reachable: [Arc<AtomicBool>; 2],
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Monitor {
    /// Starts monitoring the Discovery Server at `host` or `host:port`. Fails only if the
    /// address doesn't resolve; an unreachable server is retried in the background.
    pub fn start(discovery_server: &str) -> io::Result<Self> {
        let server = ds::server_addr(discovery_server)?;
        let table: Table = Default::default();
        let events: Events = Default::default();
        let stop = Arc::new(AtomicBool::new(false));
        let reachable: [Arc<AtomicBool>; 2] = Default::default();
        let threads = [Subject::Source, Subject::Receiver]
            .into_iter()
            .zip(reachable.clone())
            .map(|(subject, up)| {
                let (table, events, stop) = (table.clone(), events.clone(), stop.clone());
                std::thread::spawn(move || watch(server, subject, &table, &events, &stop, &up))
            })
            .collect();
        Ok(Monitor {
            table,
            events,
            reachable,
            stop,
            threads,
        })
    }

    /// Whether the Discovery Server is currently reachable. While it isn't, the device
    /// list shows the last known state.
    pub fn server_reachable(&self) -> bool {
        self.reachable.iter().all(|r| r.load(Ordering::Relaxed))
    }

    /// Every device currently known, sources first, then by name.
    pub fn devices(&self) -> Vec<Device> {
        let mut v: Vec<Device> = self.table.lock_ok().values().cloned().collect();
        v.sort_by(|a, b| (a.kind, &a.name).cmp(&(b.kind, &b.name)));
        v
    }

    /// The next change, waiting up to `timeout`; `None` if nothing changed meanwhile.
    /// Unread changes to the same device are merged into one, so the queue stays small
    /// when the application only looks at [`devices`](Monitor::devices).
    pub fn recv(&self, timeout: Duration) -> Option<DeviceEvent> {
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

impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Not stalled by a half-dead server (M-3).
        for t in self.threads.drain(..) {
            crate::pool::join_within(t, std::time::Duration::from_millis(500));
        }
    }
}

fn kind_of(subject: Subject) -> DeviceKind {
    match subject {
        Subject::Source => DeviceKind::Source,
        Subject::Receiver => DeviceKind::Receiver,
    }
}

fn push(events: &Events, e: DeviceEvent) {
    super::queue_change(&mut events.0.lock_ok(), e);
    events.1.notify_all();
}

impl super::ChangeEvent for DeviceEvent {
    type Key = (DeviceKind, String);
    fn key(&self) -> Self::Key {
        let (DeviceEvent::Added(d) | DeviceEvent::Changed(d) | DeviceEvent::Removed(d)) = self;
        (d.kind, d.id.clone())
    }
    fn change(&self) -> super::Change {
        match self {
            DeviceEvent::Added(_) => super::Change::Added,
            DeviceEvent::Changed(_) => super::Change::Changed,
            DeviceEvent::Removed(_) => super::Change::Removed,
        }
    }
    fn into_change(self, c: super::Change) -> Self {
        let (DeviceEvent::Added(d) | DeviceEvent::Changed(d) | DeviceEvent::Removed(d)) = self;
        match c {
            super::Change::Added => DeviceEvent::Added(d),
            super::Change::Changed => DeviceEvent::Changed(d),
            super::Change::Removed => DeviceEvent::Removed(d),
        }
    }
}

/// The properties a device declares, plus NDI's source counters (§8.3.6.1).
fn wanted(item: &Element, subject: Subject) -> Vec<String> {
    let mut v: Vec<String> = item
        .child("capabilities")
        .and_then(|c| c.child("monitor_capabilities"))
        .map(|m| {
            m.children_named("property")
                .filter_map(|p| p.attr("name"))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if subject == Subject::Source {
        v.extend(ds::SOURCE_COUNTERS.iter().map(|s| s.to_string()));
    }
    v
}

fn new_device(kind: DeviceKind, id: &str, item: &Element) -> Device {
    Device {
        kind,
        id: id.to_owned(),
        name: item.child_text("name").to_owned(),
        address: item
            .child_text("address")
            .trim()
            .parse()
            .ok()
            .filter(|a: &IpAddr| !a.is_unspecified()),
        properties: BTreeMap::new(),
        sent_sample: None,
        est_fps_milli: None,
    }
}

/// Updates the frame-rate estimate from a new `video-frames-sent` value. The counter sums
/// over all receivers (§8.3.6.1), so the rate is divided by their number.
fn estimate_fps(d: &mut Device) {
    let Some(n) = d
        .get("video-frames-sent")
        .and_then(|v| v.parse::<u64>().ok())
    else {
        return;
    };
    let now = Instant::now();
    if let Some((n0, t0)) = d.sent_sample {
        let dt = now.duration_since(t0).as_secs_f64();
        if n >= n0 && dt > 0.5 {
            let receivers = d
                .get("connected-receivers")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(1)
                .max(1);
            d.est_fps_milli =
                Some(((n - n0) as f64 / dt / receivers as f64 * 1000.0).round() as u64);
        }
    }
    d.sent_sample = Some((n, now));
}

fn monitor_request(subject: Subject, id: &str, props: &[String]) -> Element {
    let mut caps = Element::new("monitor_capabilities");
    for p in props {
        caps.children
            .push(Element::new("property").with_attr("name", p));
    }
    Element::new("monitor_request")
        .with_attr("unsubscribe", "false")
        .with_child(Element::new(&format!("{}_id", subject.tag())).with_text(id))
        .with_child(Element::new("listener_id"))
        .with_child(caps)
}

/// One listener connection per subject, reconnecting until `stop`.
fn watch(
    server: SocketAddr,
    subject: Subject,
    table: &Table,
    events: &Events,
    stop: &AtomicBool,
    up: &AtomicBool,
) {
    let kind = kind_of(subject);
    let tag = subject.tag();
    let id_tag = format!("{tag}_id");
    while !stop.load(Ordering::Relaxed) {
        if let Ok(mut s) = Session::connect_with(
            server,
            subject,
            Role::Listener,
            Duration::from_secs(1),
            Duration::from_secs(3),
        ) {
            let _ = s.set_timeout(Some(Duration::from_millis(250)));
            up.store(true, Ordering::Relaxed);
            // `None`: not listed yet in this session (no `now - RELIST`, which underflows
            // on platforms whose clock starts at boot).
            let mut listed: Option<Instant> = None;
            let mut subscribed: std::collections::BTreeSet<String> = Default::default();
            'session: while !stop.load(Ordering::Relaxed) {
                if listed.is_none_or(|t| t.elapsed() >= RELIST) {
                    listed = Some(Instant::now());
                    if s.send(&Element::new(&format!("query_{tag}s"))).is_err() {
                        break;
                    }
                }
                let msgs = match s.recv() {
                    Ok(Some(m)) => m,
                    Ok(None) => break,
                    Err(e) if crate::transport::is_poll_timeout(&e) => {
                        continue;
                    }
                    Err(_) => break,
                };
                for e in msgs {
                    let name = e.name.as_str();
                    if name == tag {
                        // A new advertiser, pushed bare (§8.3.9).
                        let id = e.child_text(&id_tag).to_owned();
                        if !id.is_empty() && !table.lock_ok().contains_key(&(kind, id.clone())) {
                            let d = new_device(kind, &id, &e);
                            table.lock_ok().insert((kind, id.clone()), d.clone());
                            push(events, DeviceEvent::Added(d));
                            if s.send(&monitor_request(subject, &id, &wanted(&e, subject)))
                                .is_err()
                            {
                                break 'session;
                            }
                            subscribed.insert(id);
                        }
                    } else if name == format!("{tag}s") {
                        // The full list: add newcomers (and subscribe), drop the missing.
                        // (A set: the list's every id was looked for in a list, O(n²) at
                        // 65 536; bug run 2026-11-25.)
                        let mut seen = std::collections::HashSet::new();
                        for item in e.children_named(tag) {
                            let id = item.child_text(&id_tag).to_owned();
                            if id.is_empty() {
                                continue;
                            }
                            seen.insert(id.clone());
                            // New, or known from before a reconnect: (re)subscribe either way,
                            // since subscriptions don't survive a lost connection.
                            let known = table.lock_ok().contains_key(&(kind, id.clone()));
                            if !known {
                                let d = new_device(kind, &id, item);
                                table.lock_ok().insert((kind, id.clone()), d.clone());
                                push(events, DeviceEvent::Added(d));
                            }
                            if (!known || !subscribed.contains(&id))
                                && s.send(&monitor_request(subject, &id, &wanted(item, subject)))
                                    .is_err()
                            {
                                break 'session;
                            }
                            subscribed.insert(id.clone());
                        }
                        let gone: Vec<Device> = {
                            let mut t = table.lock_ok();
                            let ids: Vec<(DeviceKind, String)> = t
                                .keys()
                                .filter(|(k, id)| *k == kind && !seen.contains(id))
                                .cloned()
                                .collect();
                            ids.iter().filter_map(|k| t.remove(k)).collect()
                        };
                        for d in gone {
                            push(events, DeviceEvent::Removed(d));
                        }
                        // Subscriptions to devices gone dropped (they were kept for good).
                        subscribed.retain(|i| seen.contains(i));
                    } else if name == format!("remove_{tag}s") {
                        for item in e.children_named(tag) {
                            let id = item.child_text(&id_tag).to_owned();
                            subscribed.remove(&id);
                            if let Some(d) = table.lock_ok().remove(&(kind, id)) {
                                push(events, DeviceEvent::Removed(d));
                            }
                        }
                    } else if name == "monitor_events" {
                        for l in e.children_named("listener") {
                            for dev in l.children_named(tag) {
                                let Some(id) = dev.attr("id") else { continue };
                                let changed = {
                                    let mut t = table.lock_ok();
                                    let Some(d) = t.get_mut(&(kind, id.to_owned())) else {
                                        continue;
                                    };
                                    let mut any = false;
                                    for p in dev.children_named("property") {
                                        // (At most 64 names a device, values of 256 bytes:
                                        // a peer's grew it without end; bug run 2026-11-25.)
                                        if let (Some(n), Some(v)) =
                                            (p.attr("name"), p.attr("value"))
                                            && n.len() <= 64
                                            && v.len() <= 256
                                            && (d.properties.contains_key(n)
                                                || d.properties.len() < 64)
                                            && d.properties.get(n).map(String::as_str) != Some(v)
                                        {
                                            d.properties.insert(n.to_owned(), v.to_owned());
                                            if n == "video-frames-sent" {
                                                estimate_fps(d);
                                            }
                                            any = true;
                                        }
                                    }
                                    any.then(|| d.clone())
                                };
                                if let Some(d) = changed {
                                    push(events, DeviceEvent::Changed(d));
                                }
                            }
                        }
                    }
                }
            }
        }
        // Lost the server: keep the last known devices (as NDI apps do) until the next
        // full list reconciles them.
        up.store(false, Ordering::Relaxed);
        let end = Instant::now() + ds::RETRY;
        while Instant::now() < end && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ndi_source_fps_is_estimated_from_frames_sent_per_receiver() {
        let item = Element::new("source").with_child(Element::new("name").with_text("SND (NDI)"));
        let mut d = new_device(DeviceKind::Source, "id", &item);
        d.properties
            .insert("connected-receivers".into(), "2".into());
        d.properties
            .insert("video-frames-sent".into(), "100".into());
        estimate_fps(&mut d);
        assert_eq!(d.fps(), None, "needs two samples");
        // 5 s later, 300 more frames over 2 receivers: 30 fps.
        d.sent_sample = Some((100, Instant::now() - Duration::from_secs(5)));
        d.properties
            .insert("video-frames-sent".into(), "400".into());
        estimate_fps(&mut d);
        assert!((d.fps().unwrap() - 30.0).abs() < 0.1, "{:?}", d.fps());
        // An IDN device's own report wins.
        d.properties.insert("x-idn-fps".into(), "29.97".into());
        assert_eq!(d.fps(), Some(29.97));
    }
}
