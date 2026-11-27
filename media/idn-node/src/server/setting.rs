//! Changing the Node's settings.

use super::*;

impl NodeCore {
    /// Changes settings (§N.4.2). Restarts the membership when the name, server or
    /// passphrase changed, or an admin code was given.
    pub fn set(&self, c: Change) -> io::Result<()> {
        let _one_at_a_time = self.changing.lock_ok();
        let Change {
            name,
            server,
            interface,
            passphrase,
            require,
            admin_code,
            lan_password,
            lan_password_hash,
            panel_pin,
            panel_pin_hash,
            panel_site_only,
            from_site,
            site_lock,
            services_off,
            auto_site,
            welcomed,
            video,
            audio,
            capture,
            sitelink,
            aes67,
            listen,
            data,
            omt,
            captions,
            netaudio,
            routers,
            sitemanager,
            apps,
            service,
            service_now,
            service_auto,
            keep_awake,
            clock,
            clock_domain,
            clock_protocol,
            clock_priority,
            audio_rate,
            approve_programs,
            link_requests,
            program,
            update_managed_by,
        } = c;
        // Locked by the site (N4, §N.31): changes come from the Site Manager only, and the
        // lock itself too.
        if !from_site && (site_lock.is_some() || self.site_locked()) {
            return Err(locked());
        }
        let mut new_video = self.settings().video;
        for (k, v) in &video {
            new_video
                .set(k, v)
                .map_err(idn_common::err::invalid_input)?;
        }
        let mut new_audio = self.settings().audio;
        for (k, v) in &audio {
            new_audio
                .set(k, v)
                .map_err(idn_common::err::invalid_input)?;
        }
        let mut new_capture = self.settings().capture;
        for (k, v) in &capture {
            new_capture
                .set(k, v)
                .map_err(idn_common::err::invalid_input)?;
        }
        let mut new_sitelink = self.settings().sitelink;
        for (k, v) in &sitelink {
            new_sitelink
                .set(k, v)
                .map_err(idn_common::err::invalid_input)?;
        }
        let mut new_aes67 = self.settings().aes67;
        for (k, v) in &aes67 {
            new_aes67
                .set(k, v)
                .map_err(idn_common::err::invalid_input)?;
        }
        let mut new_listen = self.settings().listen;
        for (k, v) in &listen {
            new_listen
                .set(k, v)
                .map_err(idn_common::err::invalid_input)?;
        }
        let mut new_data = self.settings().data;
        for (k, v) in &data {
            new_data.set(k, v).map_err(idn_common::err::invalid_input)?;
        }
        let mut new_omt = self.settings().omt;
        for (k, v) in &omt {
            new_omt.set(k, v).map_err(idn_common::err::invalid_input)?;
        }
        let mut new_captions = self.settings().captions;
        for (k, v) in &captions {
            new_captions
                .set(k, v)
                .map_err(idn_common::err::invalid_input)?;
        }
        let mut new_netaudio = self.settings().netaudio;
        for (k, v) in &netaudio {
            new_netaudio
                .set(k, v)
                .map_err(idn_common::err::invalid_input)?;
        }
        let mut new_sitemanager = self.settings().sitemanager;
        // The Site Manager's password goes to its folder as its hash, written here (as the
        // account the Site Manager runs as), not kept in the settings.
        let mut sitemanager_password = None;
        for (k, v) in &sitemanager {
            if k == "password" {
                sitemanager_password = Some(v.clone());
                continue;
            }
            new_sitemanager
                .set(k, v)
                .map_err(idn_common::err::invalid_input)?;
        }
        // Its folder now; the password once the rest of the change is checked and saved
        // (below): a change refused for another field left a new password in place while
        // the panel said it failed.
        let sitemanager_dir = if new_sitemanager.on || sitemanager_password.is_some() {
            Some(self.make_site_manager_dir()?)
        } else {
            None
        };
        // Who installs and updates this machine's IDN Node (§N.23): checked before anything
        // changes.
        let update_managed_by = update_managed_by
            .map(|v| crate::update::managed_name(&v))
            .transpose()
            .map_err(idn_common::err::invalid_input)?;
        let mut new_apps = self.settings().apps;
        for (k, v) in &apps {
            new_apps.set(k, v).map_err(idn_common::err::invalid_input)?;
        }
        let mut new_routers = self.settings().routers;
        for (k, v) in &routers {
            new_routers
                .set(k, v)
                .map_err(idn_common::err::invalid_input)?;
        }
        // The PIN held by the site: only the Site Manager changes it (§N.20).
        if !from_site
            && (panel_pin.is_some() || panel_pin_hash.is_some() || panel_site_only.is_some())
            && (self.settings().panel_site_only || panel_site_only.is_some())
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the simple-mode PIN is set by the site's Site Manager",
            ));
        }
        let pin_hash = match (panel_pin, panel_pin_hash) {
            (Some(Some(p)), _) if !p.trim().is_empty() => {
                let p = p.trim();
                if !(4..=32).contains(&p.chars().count()) {
                    return Err(idn_common::err::invalid_input(
                        "the PIN: 4 to 32 characters",
                    ));
                }
                Some(Some(idn_open_protocol::web::hash_password(p)?))
            }
            (Some(_), _) => Some(None),
            (None, Some(h)) => Some(h.filter(|h| h.starts_with("$argon2"))),
            (None, None) => None,
        };
        // Argon2id takes a moment: before the lock.
        let lan_hash = match (lan_password, lan_password_hash) {
            (Some(Some(p)), _) if !p.is_empty() => {
                Some(Some(idn_open_protocol::web::hash_password(&p)?))
            }
            (Some(_), _) => Some(None),
            (None, Some(h)) => Some(h.filter(|h| h.starts_with("$argon2"))),
            (None, None) => None,
        };
        // The slow hashes (Argon2) made before the settings are locked: every status
        // waited behind them (deep bug run 2026-11-09).
        let psk_new = match passphrase {
            Some(v) => Some(match v.filter(|v| !v.is_empty()) {
                Some(p) => Some(SiteKey::psk(&p)?),
                None => None,
            }),
            None => None,
        };
        let sitemanager_hash = match &sitemanager_password {
            // (As the Site Manager's own `set_password`: 8.)
            Some(p) if p.chars().count() < 8 => {
                return Err(idn_common::err::invalid_input(
                    "the Site Manager's password: use at least 8 characters",
                ));
            }
            Some(p) => Some(idn_open_protocol::web::hash_password(p)?),
            None => None,
        };
        let restart = {
            let mut held = self.settings.lock_ok();
            let mut leaving_own = false;
            let before = held.clone();
            // Changed on a copy, put in place only once all of it is good and saved: an
            // error half-way (an interface, a passphrase, a service, a program action)
            // left the fields before it changed in memory, unsaved, and the next good
            // change kept them without restarting the membership.
            let mut s = before.clone();
            if let Some(n) = name.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty()) {
                s.name = n;
            }
            if let Some(h) = lan_hash {
                s.lan_password = h;
            }
            if let Some(h) = pin_hash {
                s.panel_pin = h;
            }
            if let Some(v) = panel_site_only {
                s.panel_site_only = v;
            }
            if let Some(v) = site_lock {
                s.site_lock = v;
            }
            if let Some(v) = update_managed_by {
                s.update_managed_by = v;
            }
            if let Some(v) = &services_off {
                s.services_off = ServicesOff::parse(v);
            }
            if let Some(a) = auto_site {
                s.auto_site = a;
            }
            if let Some(w) = welcomed {
                s.welcomed = w;
            }
            if let Some(v) = server {
                s.server = v.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
            }
            // Leaving the site this machine runs (its server its own Site Manager, the
            // appliance's "new site here", §N.11.7) for another or none: that site's
            // credentials let go and its Site Manager off (kept, the other site was never
            // joined, its CA not the pinned one, and the old site went on announcing
            // itself; bug round 2026-11-09).
            if s.server != before.server
                && before.server.as_deref() == Some(super::site_here::OWN_SERVER)
                && before.sitemanager.on
                && sitemanager.is_empty()
            {
                s.sitemanager.on = false;
                leaving_own = true;
            }
            if let Some(v) = interface {
                let v = v.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
                if let Some(a) = &v
                    && a.parse::<std::net::Ipv4Addr>().is_err()
                {
                    return Err(idn_common::err::invalid_input(format!(
                        "interface {a:?}: give an IPv4 address of this machine"
                    )));
                }
                s.interface = v;
            }
            if let Some(k) = psk_new {
                s.psk = k;
            }
            if let Some(r) = require {
                s.require = r;
            }
            if !video.is_empty() {
                s.video = new_video;
            }
            if !audio.is_empty() {
                s.audio = new_audio;
            }
            if !capture.is_empty() {
                s.capture = new_capture;
            }
            if !sitelink.is_empty() {
                s.sitelink = new_sitelink;
            }
            if !aes67.is_empty() {
                s.aes67 = new_aes67;
            }
            if !listen.is_empty() {
                s.listen = new_listen;
            }
            if !data.is_empty() {
                s.data = new_data;
            }
            if !omt.is_empty() {
                s.omt = new_omt;
            }
            if !captions.is_empty() {
                s.captions = new_captions;
            }
            if !netaudio.is_empty() {
                s.netaudio = new_netaudio;
            }
            if !routers.is_empty() {
                s.routers = new_routers;
            }
            if !sitemanager.is_empty() {
                s.sitemanager = new_sitemanager;
            }
            if !apps.is_empty() {
                s.apps = new_apps;
            }
            // Whether a service starts by itself: the microphone's and speaker's own
            // switch, the switcher's, mixer's and display service's autostart, else the
            // switched-off list.
            let set_auto = |s: &mut Settings, id: &str, on: bool| -> io::Result<()> {
                match id {
                    "mic" => s.audio.mic = on,
                    "speaker" => s.audio.speaker = on,
                    "switcher" => s.apps.switcher = on,
                    "mixer" => s.apps.mixer = on,
                    "display" => s.apps.display = on,
                    _ if s.services().iter().any(|x| x.id == id) => {
                        if on {
                            s.services_off.0.remove(id);
                        } else {
                            s.services_off.0.insert(id.to_owned());
                        }
                    }
                    _ => {
                        return Err(idn_common::err::invalid_input(format!("no service {id:?}")));
                    }
                }
                Ok(())
            };
            if service.is_some() || service_auto.is_some() || service_now.is_some() {
                self.service_changes.fetch_add(1, Ordering::Relaxed);
            }
            if let Some((id, on)) = &service {
                set_auto(&mut s, id, *on)?;
                s.services_now.0.remove(id);
            }
            if let Some((id, on)) = &service_auto {
                set_auto(&mut s, id, *on)?;
            }
            if let Some((id, on)) = &service_now {
                if !s.services().iter().any(|x| x.id == *id) {
                    return Err(idn_common::err::invalid_input(format!("no service {id:?}")));
                }
                s.services_now.0.insert(id.clone(), *on);
            }
            if let Some(v) = keep_awake {
                s.keep_awake = v;
            }
            // A service taken out of the settings is forgotten switched off too.
            let ids: std::collections::BTreeSet<String> =
                s.services().into_iter().map(|x| x.id).collect();
            s.services_off.0.retain(|id| ids.contains(id));
            s.services_now.0.retain(|id, _| ids.contains(id));
            if let Some(v) = clock {
                s.clock.on = v;
            }
            if let Some(v) = clock_domain {
                s.clock.domain = v;
            }
            if let Some(v) = clock_protocol {
                s.clock.protocol = v;
            }
            if let Some(v) = clock_priority {
                s.clock.priority1 = v;
            }
            if let Some(v) = audio_rate {
                s.audio_rate = v;
            }
            if let Some(v) = approve_programs {
                s.approve_programs = v;
            }
            if let Some(v) = link_requests {
                s.link_requests = v;
            }
            if let Some((action, path, app)) = &program {
                // Kept one a line, `action TAB path TAB name`: a line break or tab in
                // either would add a rule of its own after a restart (a program waiting
                // for approval named itself so; bug run 2026-11-24).
                if path.chars().chain(app.chars()).any(char::is_control) {
                    return Err(idn_common::err::invalid_input(
                        "program: no control characters in its path or name",
                    ));
                }
                s.programs.retain(|r| r.path != *path);
                match action.as_str() {
                    "allow" | "block" => s.programs.push(ProgramRule {
                        path: path.clone(),
                        allow: action == "allow",
                        app: app.clone(),
                    }),
                    "forget" => {}
                    other => {
                        return Err(idn_common::err::invalid_input(format!(
                            "program: {other:?}: allow, block or forget"
                        )));
                    }
                }
            }
            // The Site Manager's password first: failing after the save left a site running
            // with none, and the panel's retry skipped making it (bug round 2026-11-09).
            if let (Some(dir), Some(h)) = (&sitemanager_dir, &sitemanager_hash) {
                idn_open_protocol::trust::pki::write_private(
                    &dir.join(idn_open_protocol::web::SITE_PASSWORD_FILE),
                    h.as_bytes(),
                )?;
            }
            s.save(&self.dir.join("node.xml"))?;
            if leaving_own {
                for f in ["device.crt", "ca.crt", "crl.der"] {
                    let _ = std::fs::remove_file(self.dir.join(f));
                }
            }
            let clock_changed = s.clock != before.clock
                || s.interface != before.interface
                || hosts_server(&s) != hosts_server(&before);
            let restart =
                (s.name != before.name || s.server != before.server || s.psk != before.psk)
                    || admin_code.is_some();
            *held = s;
            (restart, clock_changed)
        };
        let (restart, clock_changed) = restart;
        self.apply_library();
        self.log("settings changed");
        // "Not now" (no search) or an address typed in: a site being asked isn't any more.
        if (!self.settings().auto_site || self.settings().server.is_some())
            && self.trial_site().is_some()
        {
            *self.trial_site.lock_ok() = None;
            if !restart {
                self.rebuild_member(None);
            }
        }
        if restart {
            self.rebuild_member(admin_code);
        }
        if clock_changed {
            self.restart_clock();
            let _ = self.events.send(self.clock_element().to_xml());
        }
        let _ = self.events.send(self.identity().to_xml());
        Ok(())
    }

    /// Settings from the Site Manager (§N.11.2), applied as if set here.
    pub(super) fn apply_node_set(self: &Arc<Self>, m: &Element) -> Element {
        let mut reply =
            Element::new("idn_node_set_result").with_attr("id", m.attr("id").unwrap_or("0"));
        if let Some(v) = m.attr("clock_protocol")
            && idn_open_protocol::ptp::Protocol::parse(v).is_none()
        {
            return reply
                .with_attr("ok", "false")
                .with_attr("error", "clock_protocol: v2, v1 or both");
        }
        if let Some(v) = m.attr("audio_rate")
            && !v.is_empty()
            && idn_open_protocol::node::rate::parse(v).is_none()
        {
            return reply.with_attr("ok", "false").with_attr(
                "error",
                "audio_rate: 44100, 48000 or 96000 (empty: the site's)",
            );
        }
        // A service restarted from the site (a client's Site page): off, then on.
        if let Some(id) = m.attr("restart").filter(|i| !i.is_empty()) {
            let id = id.to_owned();
            // Only one that runs (a restart switched on a service that was off; bug run P).
            match self.settings().services().into_iter().find(|x| x.id == id) {
                None => {
                    return reply
                        .with_attr("ok", "false")
                        .with_attr("error", &format!("no service {id:?}"));
                }
                Some(x) if !x.on => {
                    return reply
                        .with_attr("ok", "false")
                        .with_attr("error", &format!("{id} isn't on: nothing to restart"));
                }
                Some(_) => {}
            }
            // (From the site: a machine the site locked takes it too; bug run 2026-11-24.)
            if let Err(e) = self.set(super::Change {
                service_now: Some((id.clone(), false)),
                from_site: true,
                ..Default::default()
            }) {
                return reply
                    .with_attr("ok", "false")
                    .with_attr("error", &e.to_string());
            }
            // (On again unless something else switched services meanwhile: someone
            // switched it off, or the Node stopped.)
            let at = self.service_changes.load(Ordering::Relaxed);
            let me = self.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(1));
                if me.service_changes.load(Ordering::Relaxed) != at {
                    return;
                }
                let _ = me.set(super::Change {
                    service_now: Some((id, true)),
                    from_site: true,
                    ..Default::default()
                });
            });
            return reply.with_attr("ok", "true");
        }
        // The site's newest release for this machine, asked for (§N.42; empty: none).
        if let Some(v) = m.attr("update_available") {
            self.take_update_available(v);
            return reply.with_attr("ok", "true");
        }
        // Identify this machine (a client's Site page): its name on every screen the display
        // service drives, for the seconds asked.
        if let Some(secs) = m.attr("identify").and_then(|s| s.parse::<u64>().ok()) {
            let secs = secs.clamp(1, 60);
            std::thread::spawn(move || identify_screens(secs));
            return reply.with_attr("ok", "true");
        }
        // An update, or back to the programs before it (§N.23).
        if let Some(u) = m.children_named("update").next() {
            return match self.take_update(u) {
                Ok(()) => reply.with_attr("ok", "true"),
                Err(e) => reply
                    .with_attr("ok", "false")
                    .with_attr("error", &e.to_string()),
            };
        }
        if m.children_named("rollback").next().is_some() {
            // Managed by another product (§N.23): the programs kept from an earlier update
            // of the Site Manager's would go back over that product's own install.
            if let Some(mb) = self.update_managed_by() {
                return reply
                    .with_attr("ok", "false")
                    .with_attr("error", &mb.rollback_refusal().to_string());
            }
            // Not while one downloads or installs: it went on and installed after the
            // rollback (bug run 2026-11-24). Rolled back once it is in.
            {
                let in_files = crate::update::from_files(&self.dir)
                    .is_some_and(|p| p.state == "downloading" || p.state == "installing");
                let mut u = self.update.lock_ok();
                if in_files
                    || u.as_ref()
                        .is_some_and(|p| p.state == "downloading" || p.state == "installing")
                {
                    return reply.with_attr("ok", "false").with_attr(
                        "error",
                        "an update is downloading or installing: roll back once it is done",
                    );
                }
                *u = None;
            }
            return match crate::update::rollback(&self.dir, self.service.load(Ordering::Relaxed)) {
                Ok(()) => reply.with_attr("ok", "true"),
                Err(e) => reply
                    .with_attr("ok", "false")
                    .with_attr("error", &e.to_string()),
            };
        }
        // A change to the network's devices (roadmap R.32 stage 1b): done here, answered
        // with what the device said.
        if let Some(d) = m.children_named("netaudio_do").next() {
            // The pool (R.32 stage 1c): its answer carries the channel and source too.
            if d.attr("op").is_some_and(|o| o.starts_with("pool_")) {
                return match self.netaudio_pool(d) {
                    Ok(attrs) => {
                        let mut r = reply.with_attr("ok", "true");
                        for (k, v) in attrs {
                            r = r.with_attr(&k, &v);
                        }
                        r
                    }
                    Err(e) => reply.with_attr("ok", "false").with_attr("error", &e),
                };
            }
            // A device's settings read (R.32 stage 2): what it has and offers.
            if d.attr("op") == Some("read_settings") {
                return match self.netaudio_read_settings(d) {
                    Ok(attrs) => {
                        let mut r = reply.with_attr("ok", "true");
                        for (k, v) in attrs {
                            r = r.with_attr(&k, &v);
                        }
                        r
                    }
                    Err(e) => reply.with_attr("ok", "false").with_attr("error", &e),
                };
            }
            return match self.netaudio_do(d) {
                Ok(said) => reply.with_attr("ok", "true").with_attr("did", &said),
                Err(e) => reply.with_attr("ok", "false").with_attr("error", &e),
            };
        }
        // The Site Manager's matrix asks for this machine's levels (idn-routing.md §A.15).
        if m.children_named("levels").next().is_some() {
            return reply
                .with_attr("ok", "true")
                .with_child(self.levels_element());
        }
        // The Site Manager asks for this machine's logs (§N.16).
        if m.children_named("logs").next().is_some() {
            reply = reply.with_attr("ok", "true");
            for l in self.log_snapshot() {
                reply = reply.with_child(l);
            }
            return reply;
        }
        // Route changes ride in the same message (idn-routing.md §A.4), all or none: the
        // whole message read and checked first, the settings (what a Node may refuse) put
        // in before the routes, floors and latencies (applied first, routes stayed in force
        // while the answer said the message failed; bug round F, F6-10).
        let parsed = (|| -> io::Result<_> {
            let routes: Vec<RouteChange> = m
                .children_named("route")
                .map(RouteChange::parse)
                .collect::<io::Result<_>>()?;
            let latencies = m
                .children_named("receiver_latency")
                .map(|l| {
                    let r = l.attr("receiver").unwrap_or("").to_owned();
                    if r.trim().is_empty() {
                        return Err(idn_common::err::invalid_input("latency: no receiver"));
                    }
                    Ok((r, receiver_latency(l)?))
                })
                .collect::<io::Result<Vec<_>>>()?;
            Ok((routes, latencies))
        })();
        let settings = m.attrs.iter().any(|(k, _)| k != "id");
        let r = parsed.and_then(|(routes, latencies)| {
            if settings {
                self.set(Change {
                    name: m.attr("name").map(str::to_owned),
                    interface: m
                        .attr("interface")
                        .map(|v| Some(v.to_owned()).filter(|v| !v.is_empty())),
                    require: m
                        .attr("require")
                        .map(|v| idn_common::text::parse_on_off(v) == Some(true)),
                    lan_password_hash: m
                        .attr("lan_password")
                        .map(|v| Some(v.to_owned()).filter(|v| !v.is_empty())),
                    panel_pin_hash: m
                        .attr("panel_pin")
                        .map(|v| Some(v.to_owned()).filter(|v| !v.is_empty())),
                    panel_site_only: m
                        .attr("panel_site_only")
                        .map(|v| idn_common::text::parse_on_off(v) == Some(true)),
                    from_site: true,
                    site_lock: m
                        .attr("site_lock")
                        .map(|v| idn_common::text::parse_on_off(v) == Some(true)),
                    services_off: m.attr("services_off").map(str::to_owned),
                    approve_programs: m
                        .attr("approve_programs")
                        .map(|v| idn_common::text::parse_on_off(v) == Some(true)),
                    video: video_edits(m),
                    audio: audio_edits(m),
                    capture: capture_edits(m),
                    sitelink: sitelink_edits(m),
                    aes67: aes67_edits(m),
                    listen: listen_edits(m),
                    data: data_edits(m),
                    omt: omt_edits(m),
                    captions: captions_edits(m),
                    netaudio: netaudio_edits(m),
                    routers: routers_edits(m),
                    // Not from the site: running a Site Manager is this machine's own choice.
                    sitemanager: Vec::new(),
                    apps: apps_edits(m),
                    service: service_switch(m),
                    clock: m.attr("clock").map(|v| !idn_common::switches::says_off(v)),
                    clock_domain: m.attr("clock_domain").and_then(|v| v.parse().ok()),
                    clock_protocol: m
                        .attr("clock_protocol")
                        .and_then(idn_open_protocol::ptp::Protocol::parse),
                    clock_priority: m.attr("clock_priority").map(|v| v.parse().ok()),
                    audio_rate: m
                        .attr("audio_rate")
                        .map(idn_open_protocol::node::rate::parse),
                    // Who updates this machine (§N.23): the Site Manager's Apply.
                    update_managed_by: m.attr("update.managed_by").map(str::to_owned),
                    ..Default::default()
                })?;
            }
            if !routes.is_empty() {
                self.apply_routes(&routes, true)?;
            }
            // Receivers' sync-group floors (§A.14): `ms` empty clears one.
            for f in m.children_named("receiver_floor") {
                self.set_receiver_floor(
                    f.attr("receiver").unwrap_or(""),
                    f.attr("ms").and_then(|v| v.parse().ok()),
                );
            }
            // Receivers' latencies (idn-routing.md §A.12): `ms` empty clears one.
            for (receiver, ms) in latencies {
                self.set_receiver_latency(&receiver, ms, true)?;
            }
            Ok(())
        });
        match r {
            Ok(()) => {
                self.log("settings changed by the Site Manager");
                reply = reply.with_attr("ok", "true");
            }
            Err(e) => {
                reply = reply
                    .with_attr("ok", "false")
                    .with_attr("error", &e.to_string());
            }
        }
        reply
    }
}

/// Asks this machine's display service (if it runs) to show its identify card on every
/// screen for `secs`.
fn identify_screens(secs: u64) {
    use std::io::Write;
    let body = format!(r#"{{"screen":"all","seconds":{secs}}}"#);
    let Ok(mut c) = std::net::TcpStream::connect_timeout(
        &([127, 0, 0, 1], idn_common::ports::DISPLAY).into(),
        std::time::Duration::from_secs(2),
    ) else {
        return;
    };
    let _ = c.set_write_timeout(Some(std::time::Duration::from_secs(2)));
    let _ = write!(
        c,
        "POST /api/v1/identify HTTP/1.1\r\nHost: 127.0.0.1\r\nX-IDN-Display: 1\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}
