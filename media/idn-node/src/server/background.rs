//! The Node's own threads: the Site Manager side (§N.11) and the once-a-second watch.

use super::*;

/// A report's text without what changes at every read with nothing having happened: the
/// site clock's `offset_ns`, `delay_ns` and `jitter_ns` and its `<map>` of site time; the
/// Node's `uptime_s`, the machine's `cpu_percent`, `gpu_percent` and `mem_free`, each
/// service's `for_s` (these
/// made an idle Node send its whole report every 2 s; they go with the
/// 30 s report, or with a change). For telling whether a report is worth sending again,
/// not for sending.
fn without_running_numbers(xml: &str) -> String {
    let mut out = xml.to_owned();
    for attr in [
        " offset_ns=\"",
        " delay_ns=\"",
        " jitter_ns=\"",
        " uptime_s=\"",
        " cpu_percent=\"",
        " gpu_percent=\"",
        " mem_free=\"",
        " for_s=\"",
    ] {
        while let Some(at) = out.find(attr) {
            let from = at + attr.len();
            let Some(len) = out[from..].find('"') else {
                break;
            };
            out.replace_range(at..from + len + 1, "");
        }
    }
    while let Some(at) = out.find("<map ") {
        let Some(len) = out[at..].find('>') else {
            break;
        };
        out.replace_range(at..at + len + 1, "");
    }
    out
}

/// The Site Manager side (§N.11): the report when something changed (at most every 2 s)
/// or every 30 s; settings it sends; and, with no server set, looking for a site.
pub(super) fn site_loop(weak: std::sync::Weak<NodeCore>) {
    let (mut last, mut sent_at, mut was_connected) = (String::new(), Instant::now(), false);
    let mut next_search = Instant::now();
    loop {
        let Some(core) = weak.upgrade() else { return };
        if core.stop.load(Ordering::Relaxed) {
            return;
        }
        // The site's file channel, once enrolled (§N.25).
        core.ensure_channel();
        // A new version's check-in, for the update's helper (§N.23).
        {
            let connected = matches!(
                core.enrollment().map(|e| e.state()),
                Some(TrustState::Enrolled { connected: true })
            );
            let updated = crate::update::check_in(
                &core.dir,
                core.settings().server.is_some(),
                connected,
                core.uptime(),
            );
            // The installer ended the session agent of whoever is at the screen; started again for
            // them now (it came back only at their next log-on; deep bug run 2026-11-09).
            #[cfg(windows)]
            if updated {
                crate::session_agent::start_for_console_user();
            }
            #[cfg(not(windows))]
            let _ = updated;
        }
        // A site found on the network and asked: kept once it approves, else forgotten.
        core.follow_trial_site();
        // Settings from the Site Manager: answered at once.
        if let Some(e) = core.enrollment() {
            if let Some(m) = e.next_node_set(Duration::from_millis(500)) {
                // Answered on the connection it came on: a new name restarts the
                // membership, and the new one isn't connected yet.
                // A change to the network's devices waits on them (seconds, for the pool):
                // on a thread of its own, this loop's reports and other settings going on
                // (J4-8); four at most at once, more answered here. A device's settings
                // (R.32 stage 2: a rate change holds its place ~27 s, a read 4-6 s) have
                // two places of their own, so they never make the matrix's subscriptions
                // and the pool's routes "busy" (bug run 2026-11-26).
                static ASKING: std::sync::atomic::AtomicUsize =
                    std::sync::atomic::AtomicUsize::new(0);
                static SETTINGS_ASKING: std::sync::atomic::AtomicUsize =
                    std::sync::atomic::AtomicUsize::new(0);
                let op = m
                    .children_named("netaudio_do")
                    .next()
                    .map(|d| d.attr("op").unwrap_or("").trim().to_owned());
                let slow = op.is_some();
                let (places, most): (&'static std::sync::atomic::AtomicUsize, usize) =
                    if op.as_deref().is_some_and(|o| {
                        o == "read_settings"
                            || idn_aoip_device::writes::Command::from_op(o).is_some()
                    }) {
                        (&SETTINGS_ASKING, 2)
                    } else {
                        (&ASKING, 4)
                    };
                if slow && places.fetch_add(1, Ordering::Relaxed) < most {
                    let (core2, e2) = (core.clone(), e.clone());
                    // (Kept here too: answered "busy" when no thread starts; bug run L1-8:
                    // moved into the closure, no answer at all.)
                    let id = m.attr("id").unwrap_or("0").to_owned();
                    let spawned = std::thread::Builder::new()
                        .name("idn-node-netaudio".into())
                        .spawn(move || {
                            // Given back however it ends (bug run N2-8: a panic kept the
                            // place; after four, every change was "busy" until a restart).
                            struct Done(&'static std::sync::atomic::AtomicUsize);
                            impl Drop for Done {
                                fn drop(&mut self) {
                                    self.0.fetch_sub(1, Ordering::Relaxed);
                                }
                            }
                            let _done = Done(places);
                            let reply = core2.apply_node_set(&m);
                            e2.send_to_authority(&reply);
                        });
                    if spawned.is_err() {
                        places.fetch_sub(1, Ordering::Relaxed);
                        e.send_to_authority(&busy(&id));
                    }
                } else if slow {
                    // One more than its places at once: not here (it would hold this
                    // loop's reports for seconds; L1-8), "busy", to be asked again.
                    places.fetch_sub(1, Ordering::Relaxed);
                    e.send_to_authority(&busy(m.attr("id").unwrap_or("0")));
                } else {
                    let reply = core.apply_node_set(&m);
                    e.send_to_authority(&reply);
                }
                // On to the report, alerts and crash reports all the same: a Site Manager
                // asking every 250 ms (an open matrix's levels) kept the wait above from
                // ever running out, and this Node's reports stopped (bug run 5).
            }
            let connected = matches!(e.state(), TrustState::Enrolled { connected: true });
            // (The report made only when it may be due: at most every 2 s.)
            if connected && (sent_at.elapsed() >= Duration::from_secs(2) || !was_connected) {
                let r = core.report();
                // (The routes' "sound arriving" counts as a change only while a matrix
                // watches: with speech it came and went every few seconds, and every Node
                // resent its whole report every 2 s, bug round 2026-11-09.)
                // (And the site clock's running numbers (its offset, delay, jitter and the
                // map of site time, new at every read) count as a change only while a
                // matrix watches: with them an idle Node sent its whole report, ~3.8 KB,
                // every 2 s for good (lab 473(c)). The clock's state, leader and settings
                // still count.)
                let x = if core.matrix_watching() {
                    r.to_xml()
                } else {
                    without_running_numbers(&r.to_xml().replace(" sig=\"1\"", ""))
                };
                let due =
                    x != last || sent_at.elapsed() >= Duration::from_secs(30) || !was_connected;
                if due {
                    e.send_to_authority(&super::status_of::with_load(r));
                    last = x;
                    sent_at = Instant::now();
                }
            }
            // Alerts, as they come (§N.18).
            if connected {
                let pending: Vec<_> = core.alerts.lock_ok().drain(..).collect();
                for a in pending {
                    let mut m = Element::new("idn_node_alert")
                        .with_attr("at", &a.at.to_string())
                        .with_attr("app", &a.app)
                        .with_attr("level", &a.level);
                    // A typed event's name and fields (§N.41).
                    if !a.event.is_empty() {
                        m = m
                            .with_attr("event", &a.event)
                            .with_attr("detail", &a.detail);
                    }
                    e.send_to_authority(&m.with_text(&a.text));
                }
            }
            // Crash reports, at the first chance (§N.16).
            if connected {
                for p in core.crashes_to_send(!was_connected) {
                    let name = p
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                        .to_owned();
                    let text = idn_open_protocol::diag::tail(&p, 256 << 10)
                        .map(|t| t.0)
                        .unwrap_or_default();
                    // crash-<program>-<date>-<time>-<pid>.txt: a program's name has hyphens.
                    let app = name
                        .trim_start_matches("crash-")
                        .trim_end_matches(".txt")
                        .rsplitn(4, '-')
                        .nth(3)
                        .unwrap_or("")
                        .to_owned();
                    e.send_to_authority(
                        &Element::new("idn_node_crash")
                            .with_attr("name", &name)
                            .with_attr("app", &app)
                            .with_text(&text),
                    );
                    let _ = std::fs::write(p.with_extension("sent"), b"");
                }
            }
            was_connected = connected;
        } else {
            drop(core);
            std::thread::sleep(Duration::from_millis(500));
            let Some(core) = weak.upgrade() else { return };
            // No server: look for a Site Manager (§N.11.4).
            let s = core.settings();
            if s.server.is_none()
                && s.auto_site
                && core.trial_site().is_none()
                && (Instant::now() >= next_search || core.search_now.swap(false, Ordering::Relaxed))
            {
                next_search = Instant::now() + Duration::from_secs(30);
                let found = idn_open_protocol::discovery::site::find_sites(Duration::from_secs(2));
                if found.len() == 1 {
                    let site = &found[0];
                    core.log(&format!(
                        "found the site {} at {} (CA {}…); asking to join (its address kept once it approves)",
                        site.name,
                        site.server(),
                        // (By characters: the announced value is anyone's text, and a byte cut
                        // through a character panicked the site loop for good.)
                        site.ca.chars().take(16).collect::<String>()
                    ));
                    core.try_site(site.server(), site.name.clone());
                } else if found.len() > 1 {
                    core.log(&format!(
                        "found {} sites on the network; choose one in the panel",
                        found.len()
                    ));
                }
                *core.sites.lock_ok() = found;
            }
        }
    }
}

/// The answer to a change to the network's devices this Node can't take now.
fn busy(id: &str) -> Element {
    Element::new("idn_node_set_result")
        .with_attr("id", id)
        .with_attr("ok", "false")
        .with_attr(
            "error",
            "busy with other network audio changes: try again in a moment",
        )
}

/// Once a second: tells programs when the identity, the options or the directory changed,
/// and logs state changes.
pub(super) fn watch(weak: std::sync::Weak<NodeCore>) {
    let (mut last_id, mut last_opts, mut last_dir, mut last_state) =
        (String::new(), 0u64, String::new(), String::new());
    let mut last_clock: Option<(idn_open_protocol::node::clock::ClockInfo, Instant)> = None;
    // A clock that couldn't start (no network yet, no multicast route) tried again: after
    // 10 s, then twice as long each time up to 5 min (bug run H: it waited for a change
    // of settings).
    let mut clock_retry: Option<(Instant, Duration)> = None;
    let mut clock_waits =
        idn_common::backoff::Backoff::new(Duration::from_secs(10), Duration::from_secs(300));
    // The problems' watch (§N.41.3): since when the Site Manager hasn't been reached, and
    // whether the clock was ever locked (a lock lost is an event; one starting isn't).
    let mut unreachable_since: Option<Instant> = None;
    let (mut clock_was_locked, mut clock_said) = (false, false);
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let Some(core) = weak.upgrade() else { return };
        if core.stop.load(Ordering::Relaxed) {
            return;
        }
        let failed = core.clock.lock_ok().2.is_some();
        match (&mut clock_retry, failed) {
            (_, false) => {
                clock_retry = None;
                clock_waits.reset();
            }
            (None, true) => clock_retry = Some((Instant::now(), clock_waits.next())),
            (Some((since, wait)), true) if since.elapsed() >= *wait => {
                *since = Instant::now();
                *wait = clock_waits.next();
                core.restart_clock();
            }
            _ => {}
        }
        let id = core.identity();
        let state = format!(
            "{} {}",
            id.attr("state").unwrap_or(""),
            id.attr("code").or(id.attr("error")).unwrap_or("")
        );
        if state != last_state {
            core.log(&format!("site: {}", state.trim()));
            last_state = state;
        }
        let x = id.to_xml();
        if x != last_id {
            let _ = core.events.send(x.clone());
            last_id = x;
        }
        // The clock to programs (idn-clock.md §C.3): when its state changes, when the
        // mapping they have would be over 1 µs off, and at least every 10 s.
        let ce = core.clock_element();
        let now = idn_open_protocol::node::clock::ClockInfo::from_element(&ce);
        let send = match &last_clock {
            None => true,
            Some((was, at)) => {
                let mono = idn_open_protocol::ptp::mono_ns();
                let off = match (was.mapping, now.mapping) {
                    (Some(a), Some(b)) => (a.site(mono) - b.site(mono)).abs() > 1_000,
                    (a, b) => a.is_some() != b.is_some(),
                };
                off || was.state != now.state
                    || was.locked != now.locked
                    || was.grandmaster != now.grandmaster
                    || at.elapsed() >= Duration::from_secs(10)
            }
        };
        if send {
            let _ = core.events.send(ce.to_xml());
            last_clock = Some((now.clone(), Instant::now()));
        }
        // The problems now, and their events (§N.41.3).
        {
            let st = id.attr("state").unwrap_or("").to_owned();
            let enrolled_off = st == "enrolled" && id.attr("connected") != Some("true");
            match (enrolled_off, unreachable_since) {
                (true, None) => unreachable_since = Some(Instant::now()),
                (false, Some(_)) => unreachable_since = None,
                _ => {}
            }
            let seen = super::problems::Seen {
                state: st,
                code_left: id.attr("code_left").and_then(|v| v.parse().ok()),
                error: id.attr("error").unwrap_or("").to_owned(),
                unreachable_for: unreachable_since.map(|t| t.elapsed()),
                sites_found: core.sites_found().len(),
                server_set: core.settings().server.is_some(),
                clock_state: now.state.clone(),
                clock_locked: now.locked,
            };
            core.watch_problems(&seen, clock_was_locked, &mut clock_said);
            clock_was_locked |= now.locked;
        }
        if let Some(e) = core.enrollment() {
            let o = e.options();
            if o.version > last_opts {
                let _ = core.events.send(o.to_element().to_xml());
                last_opts = o.version;
            }
            let d = directory_element(&e, "").to_xml();
            if d != last_dir && !e.directory().is_empty() {
                let _ = core.events.send(d.clone());
                last_dir = d;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lab 473(c): two reads of a locked clock a moment apart are the same report; a
    /// change of its state or leader is not.
    #[test]
    fn a_clocks_running_numbers_dont_make_a_new_report() {
        use idn_open_protocol::node::clock::ClockInfo;
        use idn_open_protocol::ptp::{Mapping, State, Status};
        let status = |offset: f64, mono: i64| Status {
            state: State::Slave { locked: true },
            grandmaster: [0, 0x1d, 0xc1, 0xff, 0xfe, 0x12, 0x34, 0x56],
            offset_ns: offset,
            delay_ns: offset * 2.0,
            mapping: Mapping {
                mono_ns: mono,
                site_ns: 1_794_000_000_000_000_000 + mono as i128,
                rate: 1.000_001,
            },
            two_domains: false,
        };
        let report = |s: &Status, jitter: &str| {
            // The machine's running numbers differ at each read too.
            let n = if jitter == "310" { "1" } else { "2" };
            Element::new("idn_node_report")
                .with_attr("uptime_s", &format!("6{n}"))
                .with_attr("cpu_percent", n)
                .with_attr("gpu_percent", n)
                .with_attr("name", "BOOTH")
                .with_child(
                    Element::new("machine")
                        .with_attr("mem_total", "8")
                        .with_attr("mem_free", n),
                )
                .with_child(
                    Element::new("service")
                        .with_attr("id", "display")
                        .with_attr("state", "running")
                        .with_attr("for_s", &format!("1{n}")),
                )
                .with_child(ClockInfo::element(s, 0, (319, 320)).with_attr("jitter_ns", jitter))
                .with_child(Element::new("update"))
                .to_xml()
        };
        let (a, b) = (
            report(&status(120.0, 5_000), "310"),
            report(&status(-85.0, 7_000_000), "296"),
        );
        assert_ne!(a, b);
        assert_eq!(without_running_numbers(&a), without_running_numbers(&b));
        // What is left still says what the clock is and whom it follows.
        let left = without_running_numbers(&a);
        assert!(left.contains("state=") && left.contains("gm=") && left.contains("<update"));
        assert!(!left.contains("offset_ns") && !left.contains("<map"));
        assert!(!left.contains("uptime_s") && !left.contains("mem_free"));
        assert!(
            !left.contains("for_s") && left.contains("state=\"running\""),
            "{left}"
        );
        assert!(left.contains("mem_total=\"8\""), "{left}");
        let mut other = status(120.0, 5_000);
        other.state = State::Slave { locked: false };
        assert_ne!(left, without_running_numbers(&report(&other, "310")));
    }
}
