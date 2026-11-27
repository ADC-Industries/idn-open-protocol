//! What the Node reports: its status, identity, sound and services.

use super::*;

impl NodeCore {
    /// The status element (§N.4.2): what the panel and `idn-node status` show.
    pub fn status(&self) -> Element {
        let s = self.settings();
        let id = self.identity();
        let pairs = self.pairs_status();
        let mut e = Element::new("status")
            .with_attr("node", env!("CARGO_PKG_VERSION"))
            .with_attr("name", &s.name)
            .with_attr("host", &idn_open_protocol::discovery::hostname())
            .with_attr("fp", &self.fingerprint())
            .with_attr("server", s.server.as_deref().unwrap_or(""))
            .with_attr(
                "trial_server",
                self.trial_site().map(|t| t.server).as_deref().unwrap_or(""),
            )
            .with_attr(
                "trial_name",
                self.trial_site().map(|t| t.name).as_deref().unwrap_or(""),
            )
            .with_attr("interface", s.interface.as_deref().unwrap_or(""))
            .with_attr("require", if s.require { "true" } else { "false" })
            .with_attr(
                "passphrase_set",
                if s.psk.is_some() { "true" } else { "false" },
            )
            .with_attr("panel_port", &s.panel_port.to_string())
            .with_attr("build", &idn_open_protocol::build_line())
            .with_attr(
                "lan_panel",
                if s.lan_password.is_some() {
                    "true"
                } else {
                    "false"
                },
            )
            .with_attr("lan_port", &s.lan_port.to_string())
            .with_attr("auto_site", if s.auto_site { "true" } else { "false" })
            .with_attr("welcomed", if s.welcomed { "true" } else { "false" })
            .with_attr("keep_awake", if s.keep_awake { "true" } else { "false" })
            // The machine's own rate (empty: the site's) and, as a child, the one in use.
            .with_attr(
                "audio_rate",
                &s.audio_rate.map(|r| r.to_string()).unwrap_or_default(),
            )
            .with_child({
                let (hz, from) = self.audio_rate();
                idn_open_protocol::node::rate::element(hz, from)
            })
            .with_attr(
                "vbcable_request",
                &self
                    .vbcable_request()
                    .map(|n| n.to_string())
                    .unwrap_or_default(),
            )
            .with_attr("uptime_s", &self.uptime().as_secs().to_string())
            .with_attr(
                "approve_programs",
                if s.approve_programs { "true" } else { "false" },
            )
            .with_attr(
                "link_requests",
                if s.link_requests { "true" } else { "false" },
            )
            .with_child(s.video.to_element())
            .with_child(s.audio.to_element())
            .with_child(s.capture.to_element())
            .with_child(s.sitelink.to_element())
            .with_child(s.aes67.to_element())
            .with_child(s.listen.to_element())
            .with_child(s.data.to_element())
            .with_child(s.omt.to_element())
            .with_child(s.captions.to_element())
            .with_child(s.netaudio.to_element())
            .with_child(s.routers.to_element())
            .with_child(s.sitemanager.to_element())
            .with_child(s.apps.to_element())
            .with_child(self.media.element())
            .with_child(panel_element(&s, self.site_locked()))
            .with_child(self.update_element())
            .with_child(s.services_off.element_with(&s.services_now))
            .with_child(self.clock_element())
            .with_child(self.audio_apps_element())
            .with_child(self.displays_element())
            .with_child(self.encoders_element())
            .with_child(pairs);
        for r in &s.programs {
            e = e.with_child(
                Element::new("program_rule")
                    .with_attr("path", &r.path)
                    .with_attr("allow", if r.allow { "true" } else { "false" })
                    .with_attr("app", &r.app),
            );
        }
        for site in self.sites_found() {
            e = e.with_child(
                Element::new("site")
                    .with_attr("name", &site.name)
                    .with_attr("server", &site.server())
                    .with_attr("ca", &site.ca),
            );
        }
        // (`code_left` and `join_mode`: a typed code's answer, §N.11.8; `site_devices`,
        // §N.42.)
        for k in [
            "state",
            "connected",
            "code",
            "error",
            "key_id",
            "code_left",
            "join_mode",
            "site_devices",
        ] {
            if let Some(v) = id.attr(k) {
                e = e.with_attr(k, v);
            }
        }
        if let Some(en) = self.enrollment() {
            e = e.with_attr("options_version", &en.options().version.to_string());
        }
        for c in self.clients() {
            let mut ce = Element::new("client")
                .with_attr("app", &c.app)
                .with_attr("pid", &c.pid.to_string())
                .with_attr("user", &c.user)
                .with_attr("exe", &c.exe)
                .with_attr("approval", self.approval(&c.exe).name())
                .with_attr(
                    "since",
                    &c.since
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_secs())
                        .to_string(),
                );
            for (h, r) in &c.regs {
                ce = ce.with_child(reg_element(Some(h), r));
            }
            e = e.with_child(ce);
        }
        for a in self.audio_elements() {
            e = e.with_child(a);
        }
        for x in self.service_elements() {
            e = e.with_child(x);
        }
        e
    }

    /// The report for the Site Manager (§N.11.2).
    pub fn report(&self) -> Element {
        let s = self.settings();
        let id = self.identity();
        let mut e = Element::new("idn_node_report")
            .with_attr("v", "1")
            // Answers the matrix's level asks without holding its reports up (§A.15, bug
            // round 2026-11-09: a Node of before bug run 5 stopped reporting while asked,
            // so the Site Manager asks only Nodes that say this).
            .with_attr("levels", "1")
            .with_attr("build", &idn_open_protocol::build_line())
            .with_attr("version", env!("CARGO_PKG_VERSION"))
            // Health for the site's machine list: the Node's uptime (the machine's CPU and
            // GPU use are added as it is sent: `with_load`).
            .with_attr("uptime_s", &self.uptime().as_secs().to_string())
            .with_attr("name", &s.name)
            // Its host name: the default device names its services carry (bug run I4-9).
            .with_attr("host", &idn_open_protocol::discovery::hostname())
            .with_attr("interface", s.interface.as_deref().unwrap_or(""))
            .with_attr("require", if s.require { "true" } else { "false" })
            .with_attr(
                "lan_panel",
                if s.lan_password.is_some() {
                    "true"
                } else {
                    "false"
                },
            )
            .with_attr("lan_port", &s.lan_port.to_string())
            .with_attr("state", id.attr("state").unwrap_or(""))
            .with_attr(
                "audio_rate",
                &s.audio_rate.map(|r| r.to_string()).unwrap_or_default(),
            )
            .with_child({
                let (hz, from) = self.audio_rate();
                idn_open_protocol::node::rate::element(hz, from)
            })
            .with_child(s.video.to_element())
            // The machine's network links (N5, §N.30).
            .with_child(links_element())
            // What machine it is (§N.11.2): CPU, GPUs, system, start, memory.
            .with_child(machine_element())
            .with_child(s.audio.to_element())
            .with_child(s.capture.to_element())
            .with_child(s.sitelink.to_element())
            .with_child(s.aes67.to_element())
            .with_child(s.listen.to_element())
            .with_child(s.data.to_element())
            .with_child(s.omt.to_element())
            .with_child(s.captions.to_element())
            .with_child(s.netaudio.to_element())
            .with_child(s.routers.to_element())
            .with_child(s.sitemanager.to_element())
            .with_child(s.apps.to_element())
            .with_child(self.media.element())
            .with_child(panel_element(&s, self.site_locked()))
            .with_child(self.update_element())
            .with_child(s.services_off.element_with(&s.services_now))
            .with_child(self.clock_element())
            // What the Node says is wrong (§N.41.3), for the site's roll-up.
            .with_child(self.problems_element());
        for c in self.clients() {
            let mut ce = Element::new("program")
                .with_attr("app", &c.app)
                .with_attr("pid", &c.pid.to_string());
            for r in c.regs.values() {
                ce = ce.with_child(reg_element(None, r));
            }
            e = e.with_child(ce);
        }
        for a in self.audio_elements() {
            e = e.with_child(a);
        }
        for x in self.service_elements() {
            e = e.with_child(x);
        }
        if let Some(d) = network_devices(&s) {
            // With the pool's table (R.32 stage 1c), for the Site Manager's matrix.
            e = e.with_child(d.with_child(self.netaudio_pool_element()));
        }
        e
    }

    /// The machine's transmitters and receivers with their routes (idn-routing.md §A.4):
    /// `audio_tx` per output with sound, `audio_rx` per receiver offered or with routes
    /// (a receiver nobody offers right now has every route `offline`).
    pub fn audio_elements(&self) -> Vec<Element> {
        let clients = self.clients();
        let mut out = Vec::new();
        for c in &clients {
            for r in c.regs.values().filter(|r| r.kind == "output") {
                if let Some(n) = r.channels {
                    let mut tx = Element::new("audio_tx")
                        .with_attr("name", &r.name)
                        .with_attr("channels", &n.to_string());
                    if !r.labels.is_empty() {
                        tx = tx.with_attr("labels", &labels_attr(&r.labels));
                    }
                    // Where its sound comes from, as its program said (§A.16).
                    if let Some(o) = &r.origin {
                        for (k, v) in o.attrs() {
                            tx = tx.with_attr(k, &v);
                        }
                    }
                    out.push(tx);
                }
            }
        }
        let offered: BTreeMap<String, (&Client, &Reg)> = clients
            .iter()
            .flat_map(|c| {
                c.regs
                    .values()
                    .filter(|r| r.kind == "audio_rx")
                    .map(move |r| (r.name.clone(), (c, r)))
            })
            .collect();
        let table = self.routes.lock_ok();
        let mut names: Vec<String> = offered.keys().cloned().collect();
        names.extend(table.receivers());
        names.sort();
        names.dedup();
        for name in names {
            let mut rx = Element::new("audio_rx").with_attr("name", &name);
            let reg = offered.get(&name);
            match reg {
                Some((c, r)) => {
                    rx = rx
                        .with_attr("channels", &r.channels.unwrap_or(0).to_string())
                        .with_attr("app", &c.app);
                    // What it is (§A.16; a program that says nothing: "other").
                    if !r.rx_kind.is_empty() {
                        rx = rx.with_attr("kind", &r.rx_kind);
                    }
                    if !r.labels.is_empty() {
                        rx = rx.with_attr("labels", &labels_attr(&r.labels));
                    }
                }
                None => rx = rx.with_attr("channels", "0").with_attr("app", ""),
            }
            if let Some(ms) = table.latency(&name) {
                rx = rx.with_attr("latency_ms", &ms.to_string());
            }
            // What its adapting lanes need, and the floor the site set (§A.14).
            if let Some(n) = reg.and_then(|(_, r)| r.need_ms) {
                rx = rx.with_attr("need_ms", &n.to_string());
            }
            if let Some(f) = self.floors.lock_ok().get(&name) {
                rx = rx.with_attr("floor_ms", &f.to_string());
            }
            for route in table.routes(&name) {
                let state = match reg {
                    None => "offline".to_owned(),
                    Some((_, r)) => r
                        .states
                        .get(&route.ch)
                        .cloned()
                        .unwrap_or_else(|| "connecting".to_owned()),
                };
                let mut re = Element::new("route")
                    .with_attr("ch", &route.ch.to_string())
                    .with_attr("tx", &route.tx)
                    .with_attr("tx_ch", &route.tx_ch.to_string())
                    .with_attr("state", &state);
                // A signal arriving on it (§A.15).
                if reg.is_some_and(|(_, r)| r.signal.contains(&route.ch)) {
                    re = re.with_attr("sig", "1");
                }
                // How its sound arrives (§A.16): the transport, `machine` on this
                // machine's ring.
                if let Some(v) = reg.and_then(|(_, r)| r.via.get(&route.ch)) {
                    re = re.with_attr("via", v);
                }
                rx = rx.with_child(re);
            }
            out.push(rx);
        }
        out
    }

    /// Every service with how it is doing (§N.14.4): `<service id kind title runs on
    /// state restarts? for_s? line? by?/>`, state `off`, `unsupported` (not on this system), `failed`
    /// (it said it can't run here and isn't started again; `line` says why), `unset`
    /// (can't run as set),
    /// `running`, `restarting`, `starting` or `no_session` (no session agent,
    /// `idn-session` or `idn session`, runs one).
    pub fn service_elements(&self) -> Vec<Element> {
        let node = self.node_services.lock_ok().clone();
        // The session's runner (the newest when there are several, e.g. two log-ins).
        let session: Option<(String, BTreeMap<String, BridgeState>)> = self
            .clients
            .lock_ok()
            .values()
            .rev()
            .find_map(|c| Some((c.user.clone(), c.services.clone()?)));
        self.settings()
            .services()
            .into_iter()
            .map(|sv| {
                let mut e = Element::new("service")
                    .with_attr("id", &sv.id)
                    .with_attr("kind", sv.kind)
                    .with_attr("title", &sv.title)
                    .with_attr("runs", sv.runs.name())
                    .with_attr("on", if sv.on { "true" } else { "false" })
                    .with_attr("auto", if sv.auto { "true" } else { "false" });
                let st = match sv.runs {
                    Runs::Node => node.get(&sv.id),
                    Runs::Session => session.as_ref().and_then(|(_, m)| m.get(&sv.id)),
                };
                if let Some(why) = sv.unsupported {
                    e = e.with_attr("line", why);
                }
                let state = match (sv.on, &sv.args, sv.runs, &session, st) {
                    (false, ..) => "off",
                    _ if sv.unsupported.is_some() => "unsupported",
                    (_, None, ..) => "unset",
                    (_, _, _, _, Some(st)) => {
                        e = e
                            .with_attr("restarts", &st.restarts.to_string())
                            .with_attr("for_s", &st.for_s.to_string())
                            .with_attr("line", &st.line);
                        if st.up {
                            "running"
                        } else if st.gave_up {
                            "failed"
                        } else {
                            "restarting"
                        }
                    }
                    (_, _, Runs::Session, None, None) => "no_session",
                    _ => "starting",
                };
                if sv.runs == Runs::Session
                    && let Some((user, _)) = &session
                {
                    e = e.with_attr("by", user);
                }
                e.with_attr("state", state)
            })
            .collect()
    }

    /// The `identity` element (§N.4.3).
    pub fn identity(&self) -> Element {
        let s = self.settings();
        // Before the member guard: it reads the enrollment's options.
        let rate = self.audio_rate();
        // Before the member guard: `in_site` reads it too.
        let pairs_on = !self.in_site();
        // Before the member guard: `site_locked` reads it too.
        let locked = self.site_locked();
        let m = self.member.read_ok();
        let mut e = Element::new("identity")
            .with_attr("fp", &self.device_key.fingerprint())
            .with_attr("name", &s.name)
            .with_attr("server", s.server.as_deref().unwrap_or(""))
            .with_attr("interface", s.interface.as_deref().unwrap_or(""))
            .with_attr("require", if s.require { "true" } else { "false" })
            .with_attr("device_key", &b64(&self.device_key.public))
            .with_attr("device_open_cert", &b64(self.device_open.cert_der()))
            .with_attr("link_key", &b64(&self.link.identity.public()))
            .with_attr("link_cert", &b64(self.link.identity.cert_der()))
            .with_child(idn_open_protocol::node::rate::element(rate.0, rate.1))
            .with_child(s.video.to_element())
            .with_child(s.audio.to_element())
            .with_child(s.capture.to_element())
            .with_child(s.sitelink.to_element())
            .with_child(s.aes67.to_element())
            .with_child(s.listen.to_element())
            .with_child(s.data.to_element())
            .with_child(s.omt.to_element())
            .with_child(s.captions.to_element())
            .with_child(s.netaudio.to_element())
            .with_child(s.routers.to_element())
            .with_child(s.sitemanager.to_element())
            .with_child(s.apps.to_element())
            .with_child(self.media.element())
            .with_child(panel_element(&s, locked))
            .with_child(self.update_element())
            .with_child(s.services_off.element_with(&s.services_now))
            .with_child(self.pairs.element(pairs_on, false));
        match &m.enrollment {
            Some(en) => {
                let st = en.state();
                e = e.with_attr("state", state_name(&st));
                match &st {
                    TrustState::Enrolled { connected } => {
                        e = e.with_attr("connected", if *connected { "true" } else { "false" });
                        // The site's devices (§N.42, the Launcher's count): its authority
                        // says it with the directory, asked for once.
                        en.watch_directory();
                        if let Some(n) = en.site_devices() {
                            e = e.with_attr("site_devices", &n.to_string());
                        }
                    }
                    TrustState::Pending { code } | TrustState::ConfirmCode { code } => {
                        e = e.with_attr("code", code)
                    }
                    TrustState::Error(x) => e = e.with_attr("error", x),
                    TrustState::WrongCode { left } => {
                        e = e.with_attr("code_left", &left.to_string())
                    }
                    _ => {}
                }
                // How the site takes machines (§E.10.10), as its authority said.
                if let Some(m) = en.site_join_mode() {
                    e = e.with_attr("join_mode", m.as_str());
                }
                if let Some(c) = en.creds() {
                    e = e
                        .with_attr("cert", &b64(&c.cert_der))
                        .with_attr("ca", &b64(&c.anchor.ca_der));
                    let crl = c.anchor.crl.read_ok().der.clone();
                    if !crl.is_empty() {
                        e = e.with_attr("crl", &b64(&crl));
                    }
                    // The machine's certificate for its HTTPS ports (extensions §E.10.11),
                    // for its programs' network ports.
                    if let Some(l) = en.lan_cert() {
                        e = e.with_attr("lan_cert", &b64(&l.der));
                    }
                }
            }
            None => {
                e = match &m.error {
                    Some(x) => e.with_attr("state", "error").with_attr("error", x),
                    None => e.with_attr("state", "none"),
                };
            }
        }
        if let Some(k) = &m.site {
            e = e
                .with_attr("key_id", &k.key_id)
                .with_attr("site_cert", &b64(k.cert()));
        }
        e
    }
}

/// The network's devices as this machine's idn-aoip read them (roadmap R.32 stage 1a,
/// idn-aoip-controller.md K.2): its `devices.xml`, while the network audio device is on.
/// The Node carries it as it is (no protocol code here).
fn network_devices(s: &Settings) -> Option<Element> {
    if !s.netaudio.on {
        return None;
    }
    // Current only (bug run I4-5: a stopped device's list isn't the network now).
    let text = super::netaudio_do::devices_text(s).ok()?;
    // Within the report's 1 MB (trust proto MAX_MESSAGE) with room for the rest: a
    // network that large is named, not listed.
    if text.len() > 512 << 10 {
        return Some(Element::new("network_devices").with_attr(
            "error",
            &format!(
                "{} KB of devices: more than a report carries",
                text.len() >> 10
            ),
        ));
    }
    // `commands`: the settings it offers now (stage 2: those checked in the lab, all with
    // the lab switch).
    idn_open_protocol::xml::parse(&text)
        .filter(|e| e.name == "network_devices")
        .map(|e| {
            e.with_attr("commands", &super::netaudio_settings::offered_now())
                .with_attr(
                    "unchecked",
                    if super::netaudio_settings::unchecked() {
                        "true"
                    } else {
                        "false"
                    },
                )
        })
}

/// Report `e` with the machine's CPU use since the last report sent (Linux; none
/// elsewhere yet) and its GPU's busy share, where the driver says (empty elsewhere).
/// Measured only as a report goes: made every 2 s and sent only when it changed, the
/// numbers covered the last 2 s (bug run 2026-11-24).
pub(super) fn with_load(e: Element) -> Element {
    e.with_attr(
        "cpu_percent",
        &machine_cpu_percent().map_or(String::new(), |p| format!("{p:.0}")),
    )
    .with_attr(
        "gpu_percent",
        &idn_common::gpu_load::machine_percent().map_or(String::new(), |p| format!("{p:.0}")),
    )
}

/// What machine this is (§N.11.2): `<machine cpu cpus os os_version kernel boot_time
/// mem_total mem_free><gpu name/>…</machine>`; what the system doesn't say left out.
fn machine_element() -> Element {
    let m = idn_common::machine::info();
    let mut e = Element::new("machine");
    for (k, v) in [
        ("cpu", m.cpu.clone()),
        (
            "cpus",
            if m.cpus > 0 {
                m.cpus.to_string()
            } else {
                String::new()
            },
        ),
        ("os", m.os.clone()),
        ("os_version", m.os_version.clone()),
        ("kernel", m.kernel.clone()),
        (
            "boot_time",
            m.boot_time.map(|b| b.to_string()).unwrap_or_default(),
        ),
    ] {
        if !v.is_empty() {
            e = e.with_attr(k, &v);
        }
    }
    if let Some((total, free)) = idn_common::machine::memory() {
        e = e
            .with_attr("mem_total", &total.to_string())
            .with_attr("mem_free", &free.to_string());
    } else if let Some(total) = m.memory_total {
        e = e.with_attr("mem_total", &total.to_string());
    }
    for g in &m.gpus {
        e = e.with_child(Element::new("gpu").with_attr("name", g));
    }
    e
}

/// The machine's CPU use since the last call, % of all its cores (Linux: `/proc/stat`).
fn machine_cpu_percent() -> Option<f64> {
    static LAST: std::sync::Mutex<Option<(u64, u64)>> = std::sync::Mutex::new(None);
    let text = std::fs::read_to_string("/proc/stat").ok()?;
    let line = text.lines().next()?.strip_prefix("cpu ")?;
    let v: Vec<u64> = line
        .split_whitespace()
        .filter_map(|x| x.parse().ok())
        .collect();
    // user nice system idle iowait irq softirq steal
    let idle = v.get(3)? + v.get(4).copied().unwrap_or(0);
    let total: u64 = v.iter().take(8).sum();
    let mut last = LAST.lock_ok();
    let out = last.and_then(|(li, lt)| {
        let (di, dt) = (idle.saturating_sub(li), total.saturating_sub(lt));
        (dt > 0).then(|| 100.0 * (1.0 - di as f64 / dt as f64))
    });
    *last = Some((idle, total));
    out
}

#[cfg(test)]
mod tests {
    /// The report's `<machine>` (§N.11.2): this machine's CPUs and system, its start and
    /// memory where the system says them; each GPU once, by name.
    #[test]
    fn the_machine_in_the_report() {
        let e = super::machine_element();
        assert_eq!(e.name, "machine");
        assert!(
            e.attr("cpus")
                .and_then(|v| v.parse::<u32>().ok())
                .is_some_and(|n| n >= 1)
        );
        assert!(e.attr("os").is_some_and(|v| !v.is_empty()));
        if cfg!(target_os = "linux") {
            for k in ["boot_time", "mem_total", "mem_free"] {
                assert!(
                    e.attr(k).and_then(|v| v.parse::<u64>().ok()).is_some(),
                    "{k}"
                );
            }
        }
        assert!(
            e.children_named("gpu")
                .all(|g| g.attr("name").is_some_and(|n| !n.is_empty()))
        );
    }
}
