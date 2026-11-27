//! The IDN Nodes through the authority (idn-node.md §N.11.2, §N.16, §N.23): their reports,
//! settings, routes, latencies and updates sent to them, crash reports, alerts and logs.

use super::*;

impl Authority {
    /// The IDN Nodes' latest reports, one per online Node (idn-node.md §N.11.2).
    pub fn node_reports(&self) -> Vec<NodeReport> {
        let st = self.0.state.lock_ok();
        st.node_reports
            .values()
            .map(|(fp, r, at, address)| NodeReport {
                fingerprint: fp.clone(),
                report: r.clone(),
                age: at.elapsed(),
                address: *address,
            })
            .collect()
    }

    /// Sends settings to the IDN Node `fingerprint` (idn-node.md §N.11.2: attributes
    /// `name`, `interface`, `require`, `lan_password` (an Argon2id hash, empty to turn
    /// its LAN panel off)) and waits up to `timeout` for its answer. Fails if the
    /// machine isn't online or isn't a Node.
    pub fn set_node(
        &self,
        fingerprint: &str,
        settings: &[(&str, &str)],
        timeout: Duration,
    ) -> io::Result<Element> {
        let mut e = Element::new("idn_node_set");
        for (k, v) in settings {
            e = e.with_attr(k, v);
        }
        self.node_set(fingerprint, e, timeout)
    }

    /// Sends audio route changes to the IDN Node `fingerprint` (idn-routing.md §A.4),
    /// applied together, and waits up to `timeout` for its answer.
    pub fn route_node(
        &self,
        fingerprint: &str,
        changes: &[crate::node::routing::RouteChange],
        timeout: Duration,
    ) -> io::Result<Element> {
        let mut e = Element::new("idn_node_set");
        for c in changes {
            e = e.with_child(c.to_element());
        }
        self.node_set(fingerprint, e, timeout)
    }

    /// A change to the network's devices (roadmap R.32 stage 1b): `<netaudio_do …/>` sent
    /// to the IDN Node `fingerprint`, which makes it; its answer (`ok`, `did` or `error`).
    pub fn netaudio_node(
        &self,
        fingerprint: &str,
        what: Element,
        timeout: Duration,
    ) -> io::Result<Element> {
        self.node_set(
            fingerprint,
            Element::new("idn_node_set").with_child(what),
            timeout,
        )
    }

    /// Sets (or with `None` clears) a receiver's latency on the IDN Node `fingerprint`
    /// (idn-routing.md §A.12), and waits up to `timeout` for its answer.
    pub fn latency_node(
        &self,
        fingerprint: &str,
        receiver: &str,
        ms: Option<u32>,
        timeout: Duration,
    ) -> io::Result<Element> {
        let l = Element::new("receiver_latency")
            .with_attr("receiver", receiver)
            .with_attr("ms", &ms.map(|m| m.to_string()).unwrap_or_default());
        self.node_set(
            fingerprint,
            Element::new("idn_node_set").with_child(l),
            timeout,
        )
    }

    /// Sets (or with `None` clears) a receiver's sync-group floor on the IDN Node
    /// `fingerprint` (idn-routing.md §A.14), and waits up to `timeout` for its answer.
    pub fn floor_node(
        &self,
        fingerprint: &str,
        receiver: &str,
        ms: Option<u32>,
        timeout: Duration,
    ) -> io::Result<Element> {
        let f = Element::new("receiver_floor")
            .with_attr("receiver", receiver)
            .with_attr("ms", &ms.map(|m| m.to_string()).unwrap_or_default());
        self.node_set(
            fingerprint,
            Element::new("idn_node_set").with_child(f),
            timeout,
        )
    }

    /// Sends an update (or a rollback) to the IDN Node `fingerprint` (idn-node.md §N.23):
    /// `<update …/>` or `<rollback/>`, inside `idn_node_set`; its answer.
    pub fn update_node(
        &self,
        fingerprint: &str,
        what: Element,
        timeout: Duration,
    ) -> io::Result<Element> {
        self.node_set(
            fingerprint,
            Element::new("idn_node_set").with_child(what),
            timeout,
        )
    }

    pub(super) fn node_set(
        &self,
        fingerprint: &str,
        msg: Element,
        timeout: Duration,
    ) -> io::Result<Element> {
        let (id, writers) = {
            let mut st = self.0.state.lock_ok();
            let conns: Vec<u64> = st
                .node_reports
                .iter()
                .filter(|(_, (fp, _, _, _))| fp == fingerprint)
                .map(|(c, _)| *c)
                .collect();
            let writers: Vec<_> = st
                .online
                .iter()
                .filter(|(_, c, _)| conns.contains(c))
                .map(|(_, _, w)| w.clone())
                .collect();
            let id = st.next_set;
            st.next_set += 1;
            st.node_pending.insert(id, fingerprint.to_owned());
            (id, writers)
        };
        // Whatever happens, no longer waited for (and its answer, if late, not kept).
        struct Done<'a>(&'a Authority, u64);
        impl Drop for Done<'_> {
            fn drop(&mut self) {
                let mut st = self.0.0.state.lock_ok();
                st.node_pending.remove(&self.1);
                st.node_results.remove(&self.1);
            }
        }
        let _done = Done(self, id);
        let Some(w) = writers.first() else {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "that machine isn't online, or doesn't run an IDN Node",
            ));
        };
        let e = msg.with_attr("id", &id.to_string());
        proto::send(&mut *w.lock_ok(), &e.to_xml())?;
        let end = Instant::now() + timeout;
        let (q, cv) = &self.0.events;
        loop {
            // `state` looked at with the events lock not held: `join` takes them the other
            // way round. A wake-up missed between the two is caught
            // by the 100 ms timeout.
            if let Some(r) = self.0.state.lock_ok().node_results.remove(&id) {
                return Ok(r);
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the machine didn't answer",
                ));
            }
            let g = q.lock_ok();
            drop(
                cv.wait_timeout(g, left.min(Duration::from_millis(100)))
                    .unwrap(),
            );
        }
    }

    /// The crash reports the Nodes sent (idn-node.md §N.16): (device, file name, size,
    /// when it arrived), newest first.
    pub fn crash_reports(&self) -> Vec<(String, String, u64, std::time::SystemTime)> {
        let mut out = Vec::new();
        for d in std::fs::read_dir(self.0.dir.join("reports"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let fp = d.file_name().to_string_lossy().into_owned();
            for f in std::fs::read_dir(d.path()).into_iter().flatten().flatten() {
                if let Ok(m) = f.metadata() {
                    out.push((
                        fp.clone(),
                        f.file_name().to_string_lossy().into_owned(),
                        m.len(),
                        m.modified().unwrap_or(std::time::UNIX_EPOCH),
                    ));
                }
            }
        }
        out.sort_by_key(|r| std::cmp::Reverse(r.3));
        out
    }

    /// A crash report's text.
    pub fn crash_report(&self, fingerprint: &str, name: &str) -> io::Result<String> {
        if !safe_name(fingerprint) || !safe_name(name) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad name"));
        }
        std::fs::read_to_string(self.0.dir.join("reports").join(fingerprint).join(name))
    }

    /// Keeps an alert the Site Manager raises itself (N5, idn-node.md §N.30: a machine's
    /// clock or link, a stream's loss), with the machines' own: about the machine
    /// `fingerprint` ("" for the site), from `app`, at `level` (`warning`, `ok`, …).
    pub fn add_alert(
        &self,
        fingerprint: &str,
        app: &str,
        level: &str,
        text: &str,
    ) -> io::Result<()> {
        let at = idn_common::epoch::secs();
        keep_alert(
            &self.0.dir,
            fingerprint,
            &Element::new("idn_node_alert")
                .with_attr("at", &at.to_string())
                .with_attr("app", app)
                .with_attr("level", level)
                .with_text(text),
        )
    }

    /// The machines' alerts (idn-node.md §N.18), newest first (the last 1000): when (Unix
    /// seconds), the machine's fingerprint, the program, the level, the text.
    pub fn alerts(&self) -> Vec<(u64, String, String, String, String)> {
        read_alerts(&self.0.dir)
    }

    /// The alerts with the typed events they carry (§N.41), newest first (the last 1000).
    pub fn alert_entries(&self) -> Vec<AlertEntry> {
        read_alert_entries(&self.0.dir)
    }

    /// Keeps a typed event the Site Manager raises itself (§N.41: a version mismatch, a
    /// join denied), with the machines' own: as [`add_alert`](Self::add_alert), carrying
    /// the event's name and fields.
    pub fn add_event(
        &self,
        fingerprint: &str,
        app: &str,
        level: &str,
        event: &str,
        text: &str,
        detail: &serde_json::Value,
    ) -> io::Result<()> {
        let at = idn_common::epoch::secs();
        keep_alert(
            &self.0.dir,
            fingerprint,
            &Element::new("idn_node_alert")
                .with_attr("at", &at.to_string())
                .with_attr("app", app)
                .with_attr("level", level)
                .with_attr("event", event)
                .with_attr("detail", &detail.to_string())
                .with_text(text),
        )
    }

    /// The IDN Node `fingerprint`'s transmitters' levels for the matrix's meters
    /// (idn-routing.md §A.15): `<levels><tx name db/>…</levels>`. Asking counts as
    /// watching: its programs report while asked.
    pub fn node_levels(&self, fingerprint: &str, timeout: Duration) -> io::Result<Element> {
        let r = self.node_set(
            fingerprint,
            Element::new("idn_node_set").with_child(Element::new("levels")),
            timeout,
        )?;
        Ok(r.child("levels")
            .cloned()
            .unwrap_or_else(|| Element::new("levels")))
    }

    /// Asks the IDN Node `fingerprint` for its logs (idn-node.md §N.16) and keeps them as a
    /// snapshot (the newest 20 per device): its name (`YYYYMMDD-HHMMSS`).
    pub fn collect_logs(&self, fingerprint: &str, timeout: Duration) -> io::Result<String> {
        let r = self.node_set(
            fingerprint,
            Element::new("idn_node_set").with_child(Element::new("logs")),
            timeout,
        )?;
        if r.attr("ok") != Some("true") {
            return Err(io::Error::other(
                r.attr("error").unwrap_or("the Node refused").to_owned(),
            ));
        }
        let t = crate::localtime::now();
        let stamp = format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            t.year, t.month, t.day, t.hour, t.minute, t.second
        );
        let dev = self.0.dir.join("node-logs").join(fingerprint);
        let dir = dev.join(&stamp);
        std::fs::create_dir_all(&dir)?;
        for (k, l) in r.children_named("log").enumerate() {
            let app: String = l
                .attr("app")
                .unwrap_or("")
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
                .collect();
            let head = format!(
                "# {} on {}{}\n",
                l.attr("path").unwrap_or(""),
                fingerprint,
                match (l.attr("error"), l.attr("cut")) {
                    (Some(e), _) => format!(" — not read: {e}"),
                    (None, Some("true")) => " — its newest part".to_owned(),
                    _ => String::new(),
                }
            );
            std::fs::write(
                dir.join(format!(
                    "{:02}-{}.log",
                    k + 1,
                    if app.is_empty() { "log" } else { &app }
                )),
                head + &l.text,
            )?;
        }
        // The newest 20 snapshots.
        let mut all: Vec<_> = std::fs::read_dir(&dev)?
            .flatten()
            .map(|e| e.path())
            .collect();
        all.sort();
        let extra = all.len().saturating_sub(20);
        for p in all.into_iter().take(extra) {
            let _ = std::fs::remove_dir_all(p);
        }
        Ok(stamp)
    }

    /// The log snapshots kept: (device, snapshot, its files with their sizes), newest first.
    pub fn log_snapshots(&self) -> Vec<LogSnapshot> {
        let mut out = Vec::new();
        for d in std::fs::read_dir(self.0.dir.join("node-logs"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let fp = d.file_name().to_string_lossy().into_owned();
            for s in std::fs::read_dir(d.path()).into_iter().flatten().flatten() {
                let mut files: Vec<(String, u64)> = std::fs::read_dir(s.path())
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|f| {
                        (
                            f.file_name().to_string_lossy().into_owned(),
                            f.metadata().map_or(0, |m| m.len()),
                        )
                    })
                    .collect();
                files.sort();
                out.push((
                    fp.clone(),
                    s.file_name().to_string_lossy().into_owned(),
                    files,
                ));
            }
        }
        out.sort_by(|a, b| b.1.cmp(&a.1));
        out
    }

    /// One file of a log snapshot.
    pub fn log_file(&self, fingerprint: &str, snapshot: &str, file: &str) -> io::Result<String> {
        if ![fingerprint, snapshot, file].iter().all(|n| safe_name(n)) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad name"));
        }
        std::fs::read_to_string(
            self.0
                .dir
                .join("node-logs")
                .join(fingerprint)
                .join(snapshot)
                .join(file),
        )
    }
}
