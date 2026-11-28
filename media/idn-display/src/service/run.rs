//! The service's commands (§D.8.3) run: feed, settings, identify, label, test pattern,
//! restart, the network API's switch.

use super::*;

impl Service {
    /// Runs a typed command for `by`.
    pub fn run_by(&self, c: Command, by: &str) -> Result<Value, String> {
        let mut g = self.inner.lock_ok();
        match c {
            Command::Feed { screen, source } => {
                let name = self.resolve(&g, &screen)?;
                let source = source.trim().to_owned();
                if source.len() > 256 || source.chars().any(char::is_control) {
                    return Err("source: a name (HOST (name)) or address:port".into());
                }
                let e = g.saved.entry(name.clone()).or_default();
                if e.feed != source {
                    e.feed = source.clone();
                    e.feed_by = if by.is_empty() {
                        "local".into()
                    } else {
                        by.to_owned()
                    };
                    e.feed_at = idn_common::epoch::secs();
                    self.save(&g);
                    // A new feed: a screen waiting to be started again starts at once.
                    g.came_up(&name);
                    g.list_soon();
                    self.bump(&mut g);
                } else if let Some(r) = g.running.get_mut(&name) {
                    // The same feed again: sent again, so a screen that gave up (its
                    // reconnect cap) tries again.
                    r.sent_feed = None;
                }
                Ok(json!({"screen": name, "feed": source}))
            }
            Command::Settings {
                screen,
                rotation,
                overscan,
                fit,
                audio,
                fallback,
                wall,
                mode,
                idle,
            } => {
                let name = self.resolve(&g, &screen)?;
                let mut s = g
                    .saved
                    .get(&name)
                    .map(|x| x.settings.clone())
                    .unwrap_or_default();
                if let Some(r) = rotation {
                    s.view.rotation = r;
                }
                if let Some(o) = overscan {
                    s.view.overscan = o;
                }
                if let Some(f) = fit {
                    s.view.fit = f;
                }
                if let Some(a) = audio {
                    s.audio = a;
                }
                if let Some(f) = fallback {
                    s.fallback = f;
                }
                if let Some(w) = wall {
                    s.view.wall = w;
                }
                if let Some(m) = mode {
                    // One the screen offers (when it says which).
                    if let Some(m) = &m
                        && let Some(listed) = g.screens.iter().find(|x| x.name == name)
                        && !listed.modes.is_empty()
                        && !listed
                            .modes
                            .iter()
                            .any(|x| x.width == m.width && x.height == m.height)
                    {
                        return Err(format!(
                            "mode: {name} offers no {}×{} (see its modes)",
                            m.width, m.height
                        ));
                    }
                    s.mode = m;
                }
                if let Some(i) = idle {
                    s.idle = i;
                }
                s.check()?;
                if !s.fallback.logo.trim().is_empty() {
                    let p = self.allowed(&s.fallback.logo)?;
                    if !p.is_file() || !crate::still::is_picture(&p) {
                        return Err(format!(
                            "fallback: logo {}: a PNG or JPEG file",
                            p.display()
                        ));
                    }
                }
                if s.fallback.show == Show::Slideshow || !s.fallback.slideshow.trim().is_empty() {
                    let p = self.allowed(&s.fallback.slideshow)?;
                    if !p.is_dir() {
                        return Err(format!("fallback: slideshow {}: a folder", p.display()));
                    }
                }
                let e = g.saved.entry(name.clone()).or_default();
                if e.settings != s {
                    e.settings = s.clone();
                    self.save(&g);
                    // New settings: a screen waiting to be started again starts at once.
                    g.came_up(&name);
                    g.list_soon();
                    self.bump(&mut g);
                }
                Ok(json!({"screen": name, "settings": s}))
            }
            Command::Identify { screen, seconds } => {
                let secs = seconds.unwrap_or(5.0);
                if !(0.5..=600.0).contains(&secs) {
                    return Err("seconds: 0.5 to 600".into());
                }
                // (The screens listed again soon: one plugged in since shows up.)
                g.list_soon();
                let names: Vec<String> = match &screen {
                    ScreenRef::Name(s) if s == "all" => {
                        g.screens.iter().map(|s| s.name.clone()).collect()
                    }
                    r => vec![self.resolve(&g, r)?],
                };
                let until = Instant::now() + Duration::from_secs_f64(secs);
                for n in &names {
                    g.identify.insert(n.clone(), (until, secs));
                    let lines = self.identify_lines(&g, n);
                    // A screen already showing: at once (else when its process is up).
                    if let Some(r) = g.running.get_mut(n)
                        && r.ready.is_some()
                    {
                        let _ = r.link.send(&ToChild::Identify {
                            lines,
                            seconds: secs,
                        });
                    }
                }
                self.bump(&mut g);
                Ok(json!({"screens": names}))
            }
            Command::Forget { screen } => {
                let name = self.resolve(&g, &screen)?;
                if g.saved.remove(&name).is_some() {
                    self.save(&g);
                    self.bump(&mut g);
                }
                Ok(json!({"screen": name}))
            }
            Command::Label { screen, label } => {
                let name = self.resolve(&g, &screen)?;
                let label = label.trim().to_owned();
                if label.chars().count() > 60 || label.chars().any(char::is_control) {
                    return Err("label: at most 60 characters".into());
                }
                let e = g.saved.entry(name.clone()).or_default();
                if e.label != label {
                    e.label = label.clone();
                    self.save(&g);
                    self.bump(&mut g);
                }
                Ok(json!({"screen": name, "label": label}))
            }
            Command::TestPattern {
                screen,
                on,
                seconds,
            } => {
                if let Some(s) = seconds
                    && !(1.0..=3600.0).contains(&s)
                {
                    return Err("seconds: 1 to 3600 (none: until turned off)".into());
                }
                // (The screens listed again soon: one plugged in since shows up.)
                g.list_soon();
                let names: Vec<String> = match &screen {
                    ScreenRef::Name(s) if s == "all" => {
                        g.screens.iter().map(|s| s.name.clone()).collect()
                    }
                    r => vec![self.resolve(&g, r)?],
                };
                for n in &names {
                    if on {
                        let until = seconds.map(|s| Instant::now() + Duration::from_secs_f64(s));
                        g.test.insert(n.clone(), until);
                    } else {
                        g.test.remove(n);
                    }
                }
                self.bump(&mut g);
                Ok(json!({"screens": names, "on": on}))
            }
            Command::Restart { screen } => {
                let name = self.resolve(&g, &screen)?;
                // Its process ended; started again at the next look (its feed and
                // settings sent to the new one).
                let was = g.running.remove(&name).is_some();
                g.came_up(&name);
                self.event(&mut g, "restarted", &name, json!({"was_running": was}));
                self.bump(&mut g);
                Ok(json!({"screen": name, "was_running": was}))
            }
            Command::Lan { on } => {
                if g.lan_off == !on {
                    return Ok(json!({"on": on}));
                }
                g.lan_off = !on;
                let body =
                    serde_json::to_vec_pretty(&ServiceFile { lan_off: !on }).unwrap_or_default();
                if let Err(e) = idn_common::fsutil::write(self.dir.join(SERVICE_FILE), body) {
                    eprintln!(
                        "idn-display: {}: {e}",
                        self.dir.join(SERVICE_FILE).display()
                    );
                }
                self.bump(&mut g);
                Ok(json!({"on": on}))
            }
        }
    }

    /// A screen's number (from 1, as listed) and name, for identify.
    pub(super) fn identify_lines(&self, g: &Inner, name: &str) -> Vec<String> {
        let n = g
            .screens
            .iter()
            .position(|s| s.name == name)
            .map_or("?".to_owned(), |k| (k + 1).to_string());
        let label = g
            .saved
            .get(name)
            .map(|s| s.label.clone())
            .unwrap_or_default();
        let mut lines = vec![n];
        if label.is_empty() {
            lines.push(name.to_owned());
        } else {
            lines.push(label);
            lines.push(name.to_owned());
        }
        // The machine and its addresses: which box this is, from across the room.
        let ips: Vec<String> = idn_open_protocol::discovery::ifaces::ipv4_multicast_interfaces()
            .into_iter()
            .filter(|i| !i.loopback)
            .map(|i| i.addr.to_string())
            .take(3)
            .collect();
        lines.push(if ips.is_empty() {
            idn_open_protocol::discovery::hostname()
        } else {
            format!(
                "{} {}",
                idn_open_protocol::discovery::hostname(),
                ips.join(" ")
            )
        });
        let saved = g.saved.get(name);
        let feed = saved.map(|s| s.feed.clone()).unwrap_or_default();
        // A wall's tile: its place and the wall's group, for whoever mounts and cables
        // the screens ("Wall R1 C2 of 2×3 · wall:BOOTH").
        if let Some(w) = saved.and_then(|s| s.settings.view.wall.as_ref()) {
            lines.push(format!(
                "Wall R{} C{} of {}×{} · {}",
                w.row + 1,
                w.col + 1,
                w.rows,
                w.cols,
                crate::view::sync_group(w, &feed)
            ));
        }
        if !feed.is_empty() {
            lines.push(feed);
        }
        lines
    }
}
