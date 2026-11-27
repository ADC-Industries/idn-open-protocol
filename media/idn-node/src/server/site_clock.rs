//! The site clock (PTP) the Node runs.

use super::*;

impl NodeCore {
    /// Starts the site clock as the settings say (again after a change of them): PTP on
    /// the standard ports, or 10319/10320 where those need privileges this Node doesn't
    /// have (user mode on Linux and macOS); `IDN_CLOCK_PORTS=EVENT,GENERAL` chooses
    /// (tests), `IDN_CLOCK=off` runs none.
    pub fn restart_clock(&self) {
        let s = self.settings();
        let mut c = self.clock.lock_ok();
        c.0 = None; // the old one stops (and frees its ports) first
        c.2 = None;
        // IDN_CLOCK=off: none for this run (tests, which shouldn't join a real site's
        // clock).
        if !s.clock.on || idn_common::switches::off("IDN_CLOCK") {
            return;
        }
        let fp = self.fingerprint();
        let mut identity = [0u8; 8];
        for (i, b) in identity.iter_mut().enumerate() {
            *b = u8::from_str_radix(fp.get(i * 2..i * 2 + 2).unwrap_or("00"), 16).unwrap_or(0);
        }
        let priority1 = s
            .clock
            .priority1
            .unwrap_or(if hosts_server(&s) { 200 } else { 248 });
        let mut cfg = idn_open_protocol::ptp::ClockConfig::standard(identity, priority1);
        // A priority set by hand: told to lead, also under v1 (idn-aoip-device.md §8.4).
        cfg.leads = s.clock.priority1.is_some();
        cfg.domain = s.clock.domain;
        cfg.protocol = s.clock.protocol;
        if let Some(a) = s.interface.as_deref().and_then(|a| a.parse().ok()) {
            cfg.interface = a;
        }
        let ports: Vec<(u16, u16)> =
            match idn_common::switches::value("IDN_CLOCK_PORTS").and_then(|v| {
                let (a, b) = v.split_once(',')?;
                Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
            }) {
                Some(p) => vec![p],
                None => vec![(319, 320), (10319, 10320)],
            };
        let mut why = String::new();
        for (ev, general) in ports {
            cfg.event_port = ev;
            cfg.general_port = general;
            match idn_open_protocol::ptp::Clock::start(cfg.clone()) {
                Ok(clock) => {
                    self.log(&format!(
                        "site clock: PTP {} domain {} on ports {ev}/{general}, priority1 {}{}",
                        cfg.protocol.name(),
                        cfg.domain,
                        // A clock speaking v1 ranks itself as statime ranks v1 leaders.
                        if cfg.protocol == idn_open_protocol::ptp::Protocol::V2 || cfg.leads {
                            priority1.to_string()
                        } else {
                            "128, weakest quality (v1: follows the network's leader; set clock-priority to lead)"
                                .to_owned()
                        },
                        if ev == 319 {
                            String::new()
                        } else if why.is_empty() {
                            " (not the standard ports: no AES67 equipment will hear it)".to_owned()
                        } else {
                            // Why the standard ports failed (lab 507-A: as root it fell back
                            // with no word of why).
                            format!(
                                " (not the standard ports, {why}: no AES67 equipment will hear it)"
                            )
                        }
                    ));
                    *c = (Some(clock), (ev, general), None);
                    return;
                }
                Err(e) => why = format!("ports {ev}/{general}: {e}"),
            }
        }
        self.log(&format!("site clock: can't start: {why}"));
        c.2 = Some(why);
    }

    /// The clock's jitter (N5, §N.30): the standard deviation of its offset over the last
    /// minute, sampled once a second while it is locked (`offset`); `None` with fewer than
    /// five samples (just locked, or not locked).
    pub(super) fn jitter(&self, offset: Option<f64>) -> Option<f64> {
        let now = std::time::Instant::now();
        let mut o = self.offsets.lock_ok();
        match offset {
            Some(x) if x.is_finite() => {
                if o.back().is_none_or(|(t, _)| {
                    now.duration_since(*t) >= std::time::Duration::from_secs(1)
                }) {
                    o.push_back((now, x));
                }
            }
            // Not locked: what came before says nothing about the next lock.
            _ => o.clear(),
        }
        while o
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > std::time::Duration::from_secs(60))
        {
            o.pop_front();
        }
        if o.len() < 5 {
            return None;
        }
        let n = o.len() as f64;
        let mean = o.iter().map(|(_, x)| x).sum::<f64>() / n;
        Some((o.iter().map(|(_, x)| (x - mean).powi(2)).sum::<f64>() / n).sqrt())
    }

    /// The site clock's `<clock>` element (spec/extensions/idn-clock.md §C.3).
    pub fn clock_element(&self) -> Element {
        let s = self.settings();
        let c = self.clock.lock_ok();
        let e = match (&c.0, &c.2) {
            (Some(clock), _) => {
                let st = clock.status();
                // Said in the log once each way (bug run H: it was a `log` line nothing
                // showed).
                static TWO: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if TWO.swap(st.two_domains, std::sync::atomic::Ordering::Relaxed) != st.two_domains
                {
                    self.log(if st.two_domains {
                        "site clock: a PTP v1 leader won't follow the v2 grandmaster: the v1 equipment keeps its own time (two time domains); make one of them the leader for both"
                    } else {
                        "site clock: one time domain again"
                    });
                }
                // Lock won and lost, and a new leader, said in the log (lab lt18: the lock
                // went for 50 s with a Dante Via leader and the log said nothing).
                static LAST: std::sync::Mutex<Option<(bool, [u8; 8])>> =
                    std::sync::Mutex::new(None);
                let now = (st.state.usable(), st.grandmaster);
                let before = LAST.lock_ok().replace(now);
                // Lines spaced: a flapping pair said once in 30 s, then the state it's in
                // (bug run K4-4).
                static SAID: std::sync::Mutex<Option<std::time::Instant>> =
                    std::sync::Mutex::new(None);
                let spaced = || {
                    let mut t = SAID.lock_ok();
                    let ok = t.is_none_or(|t| t.elapsed() >= Duration::from_secs(30));
                    if ok {
                        *t = Some(std::time::Instant::now());
                    }
                    ok
                };
                if let Some((was, gm)) = before {
                    let leader = idn_open_protocol::ptp::identity_str(&now.1);
                    let off = format!("{:.0} µs", st.offset_ns / 1000.0);
                    // One line for a takeover (K4-4: "following a new leader" and
                    // "following X now" both).
                    let takeover = was != now.0
                        && !now.0
                        && gm != now.1
                        && matches!(st.state, idn_open_protocol::ptp::State::Slave { .. });
                    if gm != now.1
                        && !takeover
                        && matches!(st.state, idn_open_protocol::ptp::State::Slave { .. })
                        && spaced()
                    {
                        self.log(&format!("site clock: following a new leader, {leader}"));
                    }
                    // A lock won is always said (the line people look for); losses and
                    // takeovers spaced.
                    // A change held back by the spacing: the state it's in said once the
                    // 30 s are up (bug run L1-9: the log's last word was "locked" while
                    // unlocked).
                    static HIDDEN: std::sync::atomic::AtomicBool =
                        std::sync::atomic::AtomicBool::new(false);
                    let changed = was != now.0;
                    if changed && !now.0 && !spaced() {
                        HIDDEN.store(true, std::sync::atomic::Ordering::Relaxed);
                    } else if changed {
                        HIDDEN.store(false, std::sync::atomic::Ordering::Relaxed);
                    }
                    if !changed && HIDDEN.load(std::sync::atomic::Ordering::Relaxed) && spaced() {
                        HIDDEN.store(false, std::sync::atomic::Ordering::Relaxed);
                        self.log(&if now.0 {
                            format!("site clock: locked to {leader} (offset {off})")
                        } else {
                            format!("site clock: not locked ({})", st.state.name())
                        });
                    }
                    if changed && (now.0 || !HIDDEN.load(std::sync::atomic::Ordering::Relaxed)) {
                        self.log(&match (now.0, st.state) {
                            (true, idn_open_protocol::ptp::State::Master) => {
                                "site clock: leading".to_owned()
                            }
                            (true, _) => format!("site clock: locked to {leader} (offset {off})"),
                            // It led, and now another does (not a lost lock; bug run J4-9).
                            (false, idn_open_protocol::ptp::State::Slave { .. })
                                if gm != now.1 =>
                            {
                                format!("site clock: following {leader} now (not locked yet)")
                            }
                            (false, idn_open_protocol::ptp::State::Slave { .. }) => format!(
                                "site clock: lost its lock to {leader} (offset {off}); programs on site time wait for it"
                            ),
                            (false, s) => format!("site clock: not usable ({})", s.name()),
                        });
                    }
                }
                let e =
                    idn_open_protocol::node::clock::ClockInfo::element(&st, s.clock.domain, c.1);
                match self.jitter(st.state.usable().then_some(st.offset_ns)) {
                    Some(j) => e.with_attr("jitter_ns", &format!("{j:.0}")),
                    None => e,
                }
            }
            // No PTP (its ports failed, or the setting is off): this machine's own clock,
            // so its programs still play in line (R.30 step 5). IDN_CLOCK=off: none.
            (None, Some(why)) => self.machine_clock().with_attr("error", why),
            (None, None) if !idn_common::switches::off("IDN_CLOCK") => self.machine_clock(),
            (None, None) => Element::new("clock").with_attr("state", "off"),
        };
        // The settings too, whatever runs (a form filled from a state without them put a
        // v1 Node back on v2, and a clock switched off back on).
        let e = e
            .with_attr("protocol", s.clock.protocol.name())
            .with_attr("enabled", if s.clock.on { "true" } else { "false" });
        // The domain set, also with no PTP running (a form filled from a state without it
        // put the machine on domain 0: bug run H).
        if e.attr("domain").is_none() {
            e.with_attr("domain", &s.clock.domain.to_string())
        } else {
            e
        }
    }

    /// This machine's own clock as site time (idn-clock.md §C.3, state `machine`): its
    /// monotonic clock mapped to its system time (as TAI), the grandmaster named after the
    /// machine, so programs here stamp their blocks and play each other's by time, and
    /// programs elsewhere (another grandmaster) by their fill. Sent again as the system
    /// time is slewed (the push's 1 µs check).
    fn machine_clock(&self) -> Element {
        let mono = idn_open_protocol::ptp::mono_ns();
        let unix = idn_common::epoch::now().as_nanos() as i128;
        let site = unix + idn_open_protocol::ptp::UTC_OFFSET_S as i128 * 1_000_000_000;
        let fp = self.fingerprint();
        Element::new("clock")
            .with_attr("state", "machine")
            .with_attr("locked", "true")
            .with_attr("gm", &format!("machine-{}", fp.get(..16).unwrap_or(&fp)))
            .with_child(
                Element::new("map")
                    .with_attr("mono_ns", &mono.to_string())
                    .with_attr("site_ns", &site.to_string())
                    .with_attr("rate", "1.000000000000"),
            )
    }
}
