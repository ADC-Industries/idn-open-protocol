//! Site monitoring (N5, idn-node.md §N.30): the machines' clocks and network links from
//! their reports, every stream from the site's own Discovery Server (the monitoring
//! channel, idn-extensions.md §E.5.1), alerts when something goes wrong and again when
//! it is right (into the site's alerts, §N.18), and the same as metrics for a
//! Prometheus-style collector on the site's network (`GET /metrics`, with a token).

use idn_common::sync::LockOk as _;
use idn_open_protocol::discovery::{Device, DeviceKind, Monitor};
use idn_open_protocol::trust::{Authority, NodeReport};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A clock this far off the site's time is alerted (1 ms: a frame is 16–40).
pub const OFFSET_ALERT_NS: f64 = 1_000_000.0;
/// A clock not locked for this long is alerted.
pub const UNLOCKED_ALERT: Duration = Duration::from_secs(30);
/// After a start, the roll-up's problems become events only after this long (the machines
/// reconnect first).
pub const SETTLE: Duration = Duration::from_secs(30);
/// A stream losing more than this share of its pictures over the last minute is alerted.
pub const LOSS_ALERT: f64 = 0.01;

/// What is wrong now, by what it is about (so each is alerted once as it starts and
/// once as it ends).
type Conditions = BTreeMap<String, (String, String)>;

/// The watcher: the monitoring channel's devices, each stream's recent counters, and
/// what was last alerted.
pub struct Watch {
    monitor: Option<Monitor>,
    /// When it started: the roll-up's events wait [`SETTLE`] (the machines reconnect
    /// after a restart; none is "offline" for that).
    started: Instant,
    state: Mutex<WatchState>,
    /// The site's typed events (§N.41): the Site Manager's own and the machines' (passed
    /// on by their Nodes), for its push channel; its own go to its log.
    pub events: Arc<idn_open_protocol::web::problems::Events>,
}

#[derive(Default)]
struct WatchState {
    /// Receivers' (received, lost) samples, the last minute's, by device id.
    samples: BTreeMap<String, VecDeque<(Instant, u64, u64)>>,
    /// Since when each machine's clock has been unlocked, by fingerprint.
    unlocked_since: BTreeMap<String, Instant>,
    /// What was alerted as wrong and not yet as right: key → (fingerprint, text).
    alerted: Conditions,
    /// The roll-up's problems last round (§N.41.4), as `fingerprint/problem`.
    attention: std::collections::BTreeSet<String>,
    /// The machines' typed events already put on the push channel: the newest one's time,
    /// and those of that second; and whether the alerts were read once (those kept before
    /// the start aren't pushed).
    machine_events: (u64, std::collections::BTreeSet<String>),
    machine_events_read: bool,
    /// The join codes waiting last round, by id; and those used or burned since (§E.10.10):
    /// one gone otherwise expired.
    codes: std::collections::BTreeSet<u64>,
    pub(crate) codes_ended: std::collections::BTreeSet<u64>,
}

impl Watch {
    /// Watches the Discovery Server at `ds_port` on this machine (the Site Manager's own).
    pub fn start(ds_port: u16) -> Arc<Watch> {
        let monitor = match Monitor::start(&format!("127.0.0.1:{ds_port}")) {
            Ok(m) => Some(m),
            Err(e) => {
                idn_open_protocol::log::log(&format!("site monitoring: {e}"));
                None
            }
        };
        Arc::new(Watch {
            monitor,
            started: Instant::now(),
            state: Mutex::default(),
            events: idn_open_protocol::web::problems::Events::new(),
        })
    }

    /// Every device on the monitoring channel now.
    pub fn devices(&self) -> Vec<Device> {
        self.monitor
            .as_ref()
            .map(|m| m.devices())
            .unwrap_or_default()
    }

    /// One round (every few seconds): samples the streams, and alerts what changed.
    pub fn tick(&self, a: &Authority) {
        let reports = a.node_reports();
        let devices = self.devices();
        let now = Instant::now();
        let mut st = self.state.lock_ok();
        for d in devices.iter().filter(|d| d.kind == DeviceKind::Receiver) {
            if let (Some(rx), Some(lost)) = (
                counter(d, "x-idn-frames-received"),
                counter(d, "x-idn-frames-lost"),
            ) {
                let q = st.samples.entry(d.id.clone()).or_default();
                q.push_back((now, rx, lost));
                while q
                    .front()
                    .is_some_and(|(t, _, _)| now.duration_since(*t) > Duration::from_secs(60))
                {
                    q.pop_front();
                }
            }
        }
        let ids: std::collections::BTreeSet<&str> = devices.iter().map(|d| d.id.as_str()).collect();
        st.samples.retain(|k, _| ids.contains(k.as_str()));
        let mut now_wrong = Conditions::new();
        for r in &reports {
            let machine = r.report.attr("name").unwrap_or("").to_owned();
            if let Some(c) = r.report.child("clock") {
                let unlocked = clock_unlocked(c);
                if unlocked {
                    st.unlocked_since
                        .entry(r.fingerprint.clone())
                        .or_insert(now);
                } else {
                    st.unlocked_since.remove(&r.fingerprint);
                }
                let long = st
                    .unlocked_since
                    .get(&r.fingerprint)
                    .is_some_and(|t| now.duration_since(*t) >= UNLOCKED_ALERT);
                for (k, text) in clock_conditions(&machine, c, long) {
                    now_wrong.insert(
                        format!("{}/{k}", r.fingerprint),
                        (r.fingerprint.clone(), text),
                    );
                }
            }
            for (k, text) in link_conditions(&machine, &r.report) {
                now_wrong.insert(
                    format!("{}/{k}", r.fingerprint),
                    (r.fingerprint.clone(), text),
                );
            }
        }
        for d in devices.iter().filter(|d| d.kind == DeviceKind::Receiver) {
            if let Some(share) = st
                .samples
                .get(&d.id)
                .and_then(loss_share)
                .filter(|s| *s > LOSS_ALERT)
            {
                now_wrong.insert(
                    format!("stream/{}", d.name),
                    (
                        String::new(),
                        format!(
                            "{} loses {:.1}% of its pictures from {} (the last minute)",
                            d.name,
                            share * 100.0,
                            d.receiving_from().unwrap_or("its source")
                        ),
                    ),
                );
            }
        }
        // A machine not reporting now (switched off, its cable out) hasn't put anything
        // right: what was wrong with it stays so, not "right again". One the site no
        // longer has (revoked, removed) is let go quietly.
        let reporting: std::collections::BTreeSet<&str> =
            reports.iter().map(|r| r.fingerprint.as_str()).collect();
        let members: std::collections::BTreeSet<String> = a
            .devices()
            .into_iter()
            .filter(|d| !d.revoked)
            .map(|d| d.fingerprint)
            .collect();
        let held: Vec<(String, (String, String))> = st
            .alerted
            .iter()
            .filter(|(_, (fp, _))| !fp.is_empty() && !reporting.contains(fp.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (k, v) in held {
            if members.contains(&v.0) {
                now_wrong.entry(k).or_insert(v);
            } else {
                st.alerted.remove(&k);
            }
        }
        // Started: alerted as warnings; ended: as ok, with what it was. Each also a typed
        // event (§N.41) on the push channel (the alert is its log line).
        for (k, (fp, text)) in &now_wrong {
            if !st.alerted.contains_key(k) {
                let _ = a.add_alert(fp, "site monitoring", "warning", text);
                self.events
                    .put(condition_event(k, fp, "warning", text, false));
            }
        }
        let ended: Vec<(String, (String, String))> = st
            .alerted
            .iter()
            .filter(|(k, _)| !now_wrong.contains_key(*k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (k, (fp, text)) in &ended {
            let _ = a.add_alert(fp, "site monitoring", "ok", &format!("right again: {text}"));
            self.events.put(condition_event(
                k,
                fp,
                "ok",
                &format!("right again: {text}"),
                true,
            ));
        }
        st.alerted = now_wrong;
        self.attention_events(&mut st, a, &reports);
        self.machines_events(&mut st, a);
        self.code_events(&mut st, a);
    }

    /// The roll-up's problems starting and ending (§N.41.4) as typed events, but the
    /// clock's (the conditions above say it) and the Node's own (its Node sent them).
    fn attention_events(&self, st: &mut WatchState, a: &Authority, reports: &[NodeReport]) {
        if self.started.elapsed() < SETTLE {
            return;
        }
        let roll = crate::attention::rollup(
            &a.devices(),
            reports,
            &crate::attention::own_version(),
            &Value::Null,
        );
        let mut now = std::collections::BTreeSet::new();
        let mut first = std::collections::BTreeMap::new();
        for m in roll["attention"].as_array().into_iter().flatten() {
            let fp = m["fingerprint"].as_str().unwrap_or("");
            for p in m["problems"].as_array().into_iter().flatten() {
                let name = p["problem"].as_str().unwrap_or("");
                if !matches!(
                    name,
                    "offline"
                        | "service_failed"
                        | "update_failed"
                        | "update_rolled_back"
                        | "version_mismatch"
                ) {
                    continue;
                }
                let key = format!("{fp}/{name}");
                if !st.attention.contains(&key) {
                    first.insert(
                        key.clone(),
                        (fp.to_owned(), m["machine"].clone(), p.clone()),
                    );
                }
                now.insert(key);
            }
        }
        for (_, (fp, machine, p)) in first {
            let mut f = p.clone();
            if let Some(o) = f.as_object_mut() {
                for k in ["problem", "level", "text"] {
                    o.remove(k);
                }
            }
            f["fingerprint"] = json!(fp);
            f["machine"] = machine;
            self.events.emit(
                p["problem"].as_str().unwrap_or(""),
                p["level"].as_str().unwrap_or("warning"),
                p["text"].as_str().unwrap_or(""),
                f,
            );
        }
        for key in st.attention.difference(&now) {
            let (fp, name) = key.split_once('/').unwrap_or(("", key));
            self.events.emit(
                name,
                "ok",
                &format!("{name}: over"),
                json!({"fingerprint": fp, "over": true}),
            );
        }
        st.attention = now;
    }

    /// The machines' typed events, as their Nodes passed them on into the site's alerts:
    /// onto the push channel (with the machine's fingerprint), each once.
    fn machines_events(&self, st: &mut WatchState, a: &Authority) {
        let entries = a.alert_entries();
        let first = !std::mem::replace(&mut st.machine_events_read, true);
        let (since, seen) = &mut st.machine_events;
        if first {
            // What was kept before this start: not new.
            *since = entries.iter().map(|x| x.at).max().unwrap_or(0);
            for x in entries.iter().filter(|x| x.at == *since) {
                seen.insert(format!(
                    "{}\u{1f}{}\u{1f}{}",
                    x.fingerprint, x.event, x.text
                ));
            }
            return;
        }
        let mut fresh: Vec<_> = entries
            .iter()
            .filter(|x| !x.event.is_empty() && x.app != "site manager" && x.at >= *since)
            .filter(|x| {
                x.at > *since
                    || !seen.contains(&format!(
                        "{}\u{1f}{}\u{1f}{}",
                        x.fingerprint, x.event, x.text
                    ))
            })
            .collect();
        fresh.reverse();
        for x in fresh {
            let mut f: Value = serde_json::from_str(&x.detail).unwrap_or_else(|_| json!({}));
            f["fingerprint"] = json!(x.fingerprint);
            f["app"] = json!(x.app);
            f["at"] = json!(x.at);
            self.events.put(idn_open_protocol::web::problems::event(
                &x.event, &x.level, &x.text, f,
            ));
            if x.at > *since {
                *since = x.at;
                seen.clear();
            }
            seen.insert(format!(
                "{}\u{1f}{}\u{1f}{}",
                x.fingerprint, x.event, x.text
            ));
        }
    }

    /// A join code that left the list neither used nor burned expired (§E.10.10).
    fn code_events(&self, st: &mut WatchState, a: &Authority) {
        let now: std::collections::BTreeSet<u64> = a.join_codes().iter().map(|c| c.id).collect();
        for id in st.codes.difference(&now) {
            if !st.codes_ended.contains(id) {
                self.events.emit(
                    "join_code_expired",
                    "info",
                    &format!("join code {id} expired unused"),
                    json!({"id": id}),
                );
            }
        }
        st.codes_ended.retain(|id| now.contains(id));
        st.codes = now;
    }

    /// A join code used or burned (the authority's event): not "expired" when it leaves.
    pub fn code_ended(&self, id: u64) {
        self.state.lock_ok().codes_ended.insert(id);
    }

    /// The streams as the panel shows them: each receiver (an input somewhere) and each
    /// source with its live values.
    pub fn streams_json(&self) -> Value {
        let st = self.state.lock_ok();
        let mut rx = Vec::new();
        let mut tx = Vec::new();
        for d in self.devices() {
            match d.kind {
                DeviceKind::Receiver => rx.push(json!({
                    "name": d.name,
                    "source": d.receiving_from(),
                    "connected": d.connected(),
                    "fps": d.fps(),
                    "received": counter(&d, "x-idn-frames-received"),
                    "lost": counter(&d, "x-idn-frames-lost"),
                    "dropped": d.frames_dropped(),
                    "loss_pct": st.samples.get(&d.id).and_then(loss_share).map(|s| (s * 1000.0).round() / 10.0),
                    "rtt_ms": d.rtt_ms(),
                    "transport": d.get("x-idn-transport"),
                    "encrypted": d.encrypted(),
                })),
                DeviceKind::Source => tx.push(json!({
                    "name": d.name,
                    "fps": d.fps(),
                    "receivers": counter(&d, "connected-receivers"),
                })),
            }
        }
        json!({"reachable": self.monitor.is_some(), "receivers": rx, "sources": tx})
    }

    /// The site as metrics (Prometheus text format, version 0.0.4).
    pub fn metrics(&self, a: &Authority) -> String {
        let reports = a.node_reports();
        let st = self.state.lock_ok();
        let mut m = Metrics::default();
        for d in a.devices() {
            let r = reports.iter().find(|r| r.fingerprint == d.fingerprint);
            m.put(
                "idn_machine_online",
                "Whether the machine reports to the Site Manager",
                &[("machine", &d.name), ("fingerprint", &d.fingerprint)],
                r.is_some() as u8 as f64,
            );
        }
        for r in &reports {
            put_machine(&mut m, r);
        }
        for d in self.devices() {
            match d.kind {
                DeviceKind::Receiver => {
                    let l = [
                        ("receiver", d.name.as_str()),
                        ("source", d.receiving_from().unwrap_or("")),
                    ];
                    m.put(
                        "idn_stream_connected",
                        "Whether the input is connected to its source",
                        &l,
                        d.connected() as u8 as f64,
                    );
                    if let Some(v) = d.fps() {
                        m.put("idn_stream_fps", "Pictures a second received", &l, v);
                    }
                    if let Some(v) = counter(&d, "x-idn-frames-received") {
                        m.put(
                            "idn_stream_frames_received_total",
                            "Pictures received",
                            &l,
                            v as f64,
                        );
                    }
                    if let Some(v) = counter(&d, "x-idn-frames-lost") {
                        m.put(
                            "idn_stream_frames_lost_total",
                            "Pictures lost in the network",
                            &l,
                            v as f64,
                        );
                    }
                    if let Some(v) = d.frames_dropped() {
                        m.put(
                            "idn_stream_frames_dropped_total",
                            "Pictures dropped by a slow receiving program",
                            &l,
                            v as f64,
                        );
                    }
                    if let Some(v) = d.rtt_ms() {
                        m.put("idn_stream_rtt_ms", "Round-trip time to the source", &l, v);
                    }
                    if let Some(s) = st.samples.get(&d.id).and_then(loss_share) {
                        m.put(
                            "idn_stream_loss_ratio_1m",
                            "Share of pictures lost over the last minute",
                            &l,
                            s,
                        );
                    }
                }
                DeviceKind::Source => {
                    let l = [("source", d.name.as_str())];
                    if let Some(v) = d.fps() {
                        m.put("idn_source_fps", "Pictures a second sent", &l, v);
                    }
                    if let Some(v) = counter(&d, "connected-receivers") {
                        m.put("idn_source_receivers", "Receivers connected", &l, v as f64);
                    }
                }
            }
        }
        m.text()
    }
}

/// A site-monitoring condition as a typed event: its key's kind (`clock-unlocked` →
/// `clock_unlocked`, `link-down/eth0` → `link_down` with `link`, `stream/NAME` →
/// `stream_loss` with `stream`).
fn condition_event(key: &str, fp: &str, level: &str, text: &str, over: bool) -> Value {
    let rest = key.strip_prefix(&format!("{fp}/")).unwrap_or(key);
    let (kind, what) = rest.split_once('/').unwrap_or((rest, ""));
    let (name, field) = match kind {
        "stream" => ("stream_loss", "stream"),
        k => (k, "link"),
    };
    let mut f = json!({"fingerprint": fp});
    if !what.is_empty() {
        f[field] = json!(what);
    }
    if over {
        f["over"] = json!(true);
    }
    idn_open_protocol::web::problems::event(&name.replace('-', "_"), level, text, f)
}

/// A device's whole-number property.
fn counter(d: &Device, k: &str) -> Option<u64> {
    d.get(k).and_then(|v| v.trim().parse().ok())
}

/// The share of pictures lost over the samples (lost / (received + lost)), once they span
/// some pictures.
fn loss_share(q: &VecDeque<(Instant, u64, u64)>) -> Option<f64> {
    let (first, last) = (q.front()?, q.back()?);
    let rx = last.1.saturating_sub(first.1);
    let lost = last.2.saturating_sub(first.2);
    (rx + lost >= 30).then(|| lost as f64 / (rx + lost) as f64)
}

/// Whether a running clock isn't locked (`state` other than off or error, `locked` false).
fn clock_unlocked(c: &idn_open_protocol::xml::Element) -> bool {
    !matches!(c.attr("state"), Some("off" | "error") | None) && c.attr("locked") != Some("true")
}

/// A machine's clock's wrong conditions: not locked for long, or too far off.
pub fn clock_conditions(
    machine: &str,
    c: &idn_open_protocol::xml::Element,
    unlocked_long: bool,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if unlocked_long {
        out.push((
            "clock-unlocked".into(),
            format!("{machine}: the site clock isn't locked"),
        ));
    }
    if c.attr("locked") == Some("true")
        && let Some(o) = c.attr("offset_ns").and_then(|o| o.parse::<f64>().ok())
        && o.abs() > OFFSET_ALERT_NS
    {
        out.push((
            "clock-offset".into(),
            format!("{machine}: the site clock is {:.2} ms off", o / 1e6),
        ));
    }
    out
}

/// A machine's links' wrong conditions: down (one that has an address), or under a
/// gigabit where the system says its speed (a cable or switch port fallen back).
pub fn link_conditions(
    machine: &str,
    report: &idn_open_protocol::xml::Element,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for l in report
        .child("links")
        .into_iter()
        .flat_map(|e| e.children_named("link"))
    {
        let name = l.attr("name").unwrap_or("");
        // Network cards only: container bridges and tunnels are often down by design (11
        // Docker bridges alerted on the lab host).
        if l.attr("physical") != Some("true") {
            continue;
        }
        if l.attr("up") == Some("false") {
            out.push((
                format!("link-down/{name}"),
                format!("{machine}: network link {name} is down"),
            ));
        } else if let Some(s) = l.attr("speed_mbps").and_then(|s| s.parse::<u64>().ok())
            && s < 1000
        {
            out.push((
                format!("link-slow/{name}"),
                format!("{machine}: network link {name} runs at {s} Mbit/s (under a gigabit)"),
            ));
        }
    }
    out
}

/// One machine's clock and links as metrics.
fn put_machine(m: &mut Metrics, r: &NodeReport) {
    let machine = r.report.attr("name").unwrap_or("");
    let base = [
        ("machine", machine),
        ("fingerprint", r.fingerprint.as_str()),
    ];
    if let Some(c) = r.report.child("clock")
        && !matches!(c.attr("state"), Some("off") | None)
    {
        m.put(
            "idn_clock_locked",
            "Whether the site clock is locked",
            &base,
            (c.attr("locked") == Some("true")) as u8 as f64,
        );
        for (k, name, help) in [
            (
                "offset_ns",
                "idn_clock_offset_ns",
                "The clock's offset from the site's time",
            ),
            (
                "delay_ns",
                "idn_clock_delay_ns",
                "The path delay to the clock's leader",
            ),
            (
                "jitter_ns",
                "idn_clock_jitter_ns",
                "The offset's standard deviation over the last minute",
            ),
        ] {
            if let Some(v) = c.attr(k).and_then(|v| v.parse::<f64>().ok()) {
                m.put(name, help, &base, v);
            }
        }
    }
    for l in r
        .report
        .child("links")
        .into_iter()
        .flat_map(|e| e.children_named("link"))
    {
        let lab = [
            ("machine", machine),
            ("fingerprint", r.fingerprint.as_str()),
            ("link", l.attr("name").unwrap_or("")),
        ];
        m.put(
            "idn_link_up",
            "Whether the network link is up",
            &lab,
            (l.attr("up") == Some("true")) as u8 as f64,
        );
        if let Some(v) = l.attr("speed_mbps").and_then(|v| v.parse::<f64>().ok()) {
            m.put("idn_link_speed_mbps", "The network link's speed", &lab, v);
        }
    }
}

/// Metrics gathered by name, written in the Prometheus text format.
#[derive(Default)]
struct Metrics {
    by_name: BTreeMap<&'static str, (&'static str, Vec<String>)>,
}

impl Metrics {
    fn put(&mut self, name: &'static str, help: &'static str, labels: &[(&str, &str)], v: f64) {
        let esc = |s: &str| {
            s.replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
        };
        let l: Vec<String> = labels
            .iter()
            .map(|(k, v)| format!("{k}=\"{}\"", esc(v)))
            .collect();
        self.by_name
            .entry(name)
            .or_insert((help, Vec::new()))
            .1
            .push(format!("{name}{{{}}} {v}", l.join(",")));
    }

    fn text(&self) -> String {
        let mut out = String::new();
        for (name, (help, lines)) in &self.by_name {
            out += &format!("# HELP {name} {help}\n# TYPE {name} gauge\n");
            for l in lines {
                out += l;
                out.push('\n');
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use idn_open_protocol::xml::Element;

    #[test]
    fn a_clock_far_off_or_long_unlocked_is_wrong() {
        let ok = Element::new("clock")
            .with_attr("state", "slave")
            .with_attr("locked", "true")
            .with_attr("offset_ns", "2500");
        assert!(clock_conditions("BOOTH", &ok, false).is_empty());
        let off = Element::new("clock")
            .with_attr("state", "slave")
            .with_attr("locked", "true")
            .with_attr("offset_ns", "-1500000");
        let c = clock_conditions("BOOTH", &off, false);
        assert_eq!(c.len(), 1);
        assert!(c[0].1.contains("1.50 ms off"), "{c:?}");
        let unlocked = Element::new("clock")
            .with_attr("state", "listening")
            .with_attr("locked", "false");
        assert!(clock_unlocked(&unlocked));
        assert_eq!(
            clock_conditions("BOOTH", &unlocked, true)[0].0,
            "clock-unlocked"
        );
        assert!(!clock_unlocked(
            &Element::new("clock").with_attr("state", "off")
        ));
    }

    #[test]
    fn a_link_down_or_slow_is_wrong() {
        let r = Element::new("report").with_child(
            Element::new("links")
                .with_child(
                    Element::new("link")
                        .with_attr("name", "eth0")
                        .with_attr("physical", "true")
                        .with_attr("up", "true")
                        .with_attr("speed_mbps", "1000"),
                )
                .with_child(
                    Element::new("link")
                        .with_attr("name", "eth1")
                        .with_attr("physical", "true")
                        .with_attr("up", "true")
                        .with_attr("speed_mbps", "100"),
                )
                .with_child(
                    Element::new("link")
                        .with_attr("name", "wlan0")
                        .with_attr("physical", "true")
                        .with_attr("up", "false"),
                )
                // Virtual links aren't watched.
                .with_child(
                    Element::new("link")
                        .with_attr("name", "vpn0")
                        .with_attr("up", "false")
                        .with_attr("physical", "false"),
                )
                .with_child(
                    Element::new("link")
                        .with_attr("name", "docker0")
                        .with_attr("up", "false")
                        .with_attr("physical", "false"),
                ),
        );
        let c = link_conditions("BOOTH", &r);
        let keys: Vec<&str> = c.iter().map(|x| x.0.as_str()).collect();
        assert_eq!(keys, ["link-slow/eth1", "link-down/wlan0"]);
    }

    #[test]
    fn loss_is_a_share_of_the_last_minutes_pictures() {
        let t = Instant::now();
        let q: VecDeque<_> = [(t, 1000, 0), (t, 1990, 10)].into();
        assert_eq!(loss_share(&q), Some(0.01));
        let few: VecDeque<_> = [(t, 0, 0), (t, 10, 1)].into();
        assert_eq!(loss_share(&few), None, "too few pictures to tell");
    }

    #[test]
    fn metrics_are_prometheus_text_with_labels_escaped() {
        let mut m = Metrics::default();
        m.put("idn_x", "Help", &[("machine", "A \"B\"")], 1.5);
        m.put("idn_x", "Help", &[("machine", "C")], 2.0);
        assert_eq!(
            m.text(),
            "# HELP idn_x Help\n# TYPE idn_x gauge\nidn_x{machine=\"A \\\"B\\\"\"} 1.5\nidn_x{machine=\"C\"} 2\n"
        );
    }
}
