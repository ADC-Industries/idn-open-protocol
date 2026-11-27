//! The Node's settings: their defaults, and the file they are kept in.

use super::*;

impl Settings {
    /// Every service these settings ask for, the switched-off ones too (§N.14.4).
    pub fn services(&self) -> Vec<idn_open_protocol::node::bridges::Service> {
        idn_open_protocol::node::bridges::services(
            &idn_open_protocol::discovery::hostname(),
            super::MACHINE_RATE.load(std::sync::atomic::Ordering::Relaxed),
            &self.audio,
            &self.capture,
            &self.sitelink,
            &self.aes67,
            &self.listen,
            &self.data,
            &self.omt,
            &self.captions,
            &self.routers,
            &self.sitemanager,
            &self.apps,
            &self.netaudio,
            &self.services_off,
            &self.services_now,
        )
    }

    pub(super) fn defaults() -> Self {
        Settings {
            name: idn_open_protocol::discovery::hostname().to_uppercase(),
            server: None,
            interface: None,
            psk: None,
            require: false,
            panel_port: PANEL_PORT,
            saved_ports: None,
            lan_password: None,
            auto_site: true,
            welcomed: false,
            lan_port: LAN_PANEL_PORT,
            video: VideoSettings::default(),
            audio: AudioBridges::default(),
            capture: CaptureBridges::default(),
            sitelink: SiteLinkSetting::default(),
            aes67: Aes67Bridges::default(),
            listen: ListenSetting::default(),
            data: DataSetting::default(),
            omt: OmtBridges::default(),
            captions: CaptionsSetting::default(),
            netaudio: NetAudioSetting::default(),
            routers: RoutersSetting::default(),
            sitemanager: SiteManagerSetting::default(),
            apps: AppsSetting::default(),
            services_off: ServicesOff::default(),
            services_now: ServicesNow::default(),
            keep_awake: true,
            audio_rate: None,
            clock: ClockSettings::default(),
            approve_programs: false,
            link_requests: true,
            panel_pin: None,
            panel_site_only: false,
            site_lock: false,
            programs: Vec::new(),
            update_managed_by: None,
        }
    }

    pub(super) fn load(path: &Path) -> Self {
        let mut s = Settings::defaults();
        // (Unreadable or unparsable: set aside and said, not saved over with defaults.)
        let e = match idn_common::fsutil::load_saved(path, |b| {
            idn_open_protocol::xml::parse(std::str::from_utf8(b).ok()?)
        }) {
            Ok(Some(e)) => e,
            Ok(None) => return s,
            Err(why) => {
                idn_common::log::log_or_stderr(&format!("idn-node: settings: {why}"));
                return s;
            }
        };
        let opt = |k: &str| {
            e.attr(k)
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
        };
        if let Some(n) = opt("name") {
            s.name = n;
        }
        s.server = opt("server");
        s.interface = opt("interface");
        s.psk = e
            .attr("psk")
            .and_then(unb64)
            .and_then(|k| k.as_slice().try_into().ok());
        s.require = e.attr("require") == Some("true");
        if let Some(p) = opt("panel_port").and_then(|p| p.parse().ok()) {
            s.panel_port = p;
        }
        s.lan_password = opt("lan_password");
        if let Some(p) = opt("lan_port").and_then(|p| p.parse().ok()) {
            s.lan_port = p;
        }
        s.auto_site = e.attr("auto_site") != Some("false");
        s.welcomed = e.attr("welcomed") == Some("true");
        for k in idn_open_protocol::node::video::KEYS {
            if let Some(v) = e.attr(&format!("video_{k}")) {
                let _ = s.video.set(k, v);
            }
        }
        for k in idn_open_protocol::node::bridges::AUDIO_KEYS {
            if let Some(v) = e.attr(&format!("audio_{k}")) {
                let _ = s.audio.set(k, v);
            }
        }
        for k in ["screens", "webcams"] {
            if let Some(v) = e.attr(&format!("capture_{k}")) {
                let _ = s.capture.set(k, v);
            }
        }
        for k in idn_open_protocol::node::bridges::SITELINK_KEYS {
            if let Some(v) = e.attr(&format!("sitelink_{k}")) {
                let _ = s.sitelink.set(k, v);
            }
        }
        for k in idn_open_protocol::node::bridges::AES67_KEYS {
            if let Some(v) = e.attr(&format!("aes67_{k}")) {
                let _ = s.aes67.set(k, v);
            }
        }
        for k in idn_open_protocol::node::bridges::LISTEN_KEYS {
            if let Some(v) = e.attr(&format!("listen_{k}")) {
                let _ = s.listen.set(k, v);
            }
        }
        for k in idn_open_protocol::node::bridges::DATA_KEYS {
            if let Some(v) = e.attr(&format!("data_{k}")) {
                let _ = s.data.set(k, v);
            }
        }
        for k in idn_open_protocol::node::bridges::OMT_KEYS {
            if let Some(v) = e.attr(&format!("omt_{k}")) {
                let _ = s.omt.set(k, v);
            }
        }
        for k in idn_open_protocol::node::bridges::CAPTIONS_KEYS {
            if let Some(v) = e.attr(&format!("captions_{k}")) {
                let _ = s.captions.set(k, v);
            }
        }
        for k in idn_open_protocol::node::bridges::NETAUDIO_KEYS {
            if let Some(v) = e.attr(&format!("netaudio_{k}")) {
                let _ = s.netaudio.set(k, v);
            }
        }
        if let Some(v) = e.attr("routers_list") {
            let _ = s.routers.set("list", v);
        }
        for k in idn_open_protocol::node::bridges::SITE_MANAGER_KEYS {
            if let Some(v) = e.attr(&format!("sitemanager_{k}")) {
                let _ = s.sitemanager.set(k, v);
            }
        }
        for k in idn_open_protocol::node::bridges::APPS_KEYS {
            if let Some(v) = e.attr(&format!("apps_{k}")) {
                let _ = s.apps.set(k, v);
            }
        }
        s.services_off = ServicesOff::parse(e.attr("services_off").unwrap_or(""));
        s.keep_awake = e.attr("keep_awake") != Some("false");
        s.clock.on = e.attr("clock") != Some("off");
        s.clock.domain = opt("clock_domain")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        s.clock.priority1 = opt("clock_priority").and_then(|v| v.parse().ok());
        s.clock.protocol = opt("clock_protocol")
            .and_then(|v| idn_open_protocol::ptp::Protocol::parse(&v))
            .unwrap_or_default();
        s.audio_rate = opt("audio_rate").and_then(|v| idn_open_protocol::node::rate::parse(&v));
        s.approve_programs = e.attr("approve_programs") == Some("true");
        s.link_requests = e.attr("link_requests") != Some("false");
        s.programs = parse_rules(e.attr("programs").unwrap_or(""));
        s.panel_pin = e
            .attr("panel_pin")
            .filter(|h| h.starts_with("$argon2"))
            .map(str::to_owned);
        s.panel_site_only = e.attr("panel_site_only") == Some("true");
        s.site_lock = e.attr("site_lock") == Some("true");
        // (A bad name, as a hand edit could leave: not managed.)
        s.update_managed_by = e
            .attr("update_managed_by")
            .and_then(|v| crate::update::managed_name(v).ok().flatten());
        s
    }

    pub(super) fn save(&self, path: &Path) -> io::Result<()> {
        let (panel_port, lan_port) = self.saved_ports.unwrap_or((self.panel_port, self.lan_port));
        let e = Element::new("idn_node")
            .with_attr("name", &self.name)
            .with_attr("server", self.server.as_deref().unwrap_or(""))
            .with_attr("interface", self.interface.as_deref().unwrap_or(""))
            .with_attr("psk", &self.psk.map(|k| b64(&k)).unwrap_or_default())
            .with_attr("require", if self.require { "true" } else { "false" })
            .with_attr("panel_port", &panel_port.to_string())
            .with_attr("lan_password", self.lan_password.as_deref().unwrap_or(""))
            .with_attr("auto_site", if self.auto_site { "true" } else { "false" })
            .with_attr("welcomed", if self.welcomed { "true" } else { "false" })
            .with_attr("lan_port", &lan_port.to_string())
            .with_attr("clock", if self.clock.on { "on" } else { "off" })
            .with_attr("clock_domain", &self.clock.domain.to_string())
            .with_attr("clock_protocol", self.clock.protocol.name())
            .with_attr(
                "audio_rate",
                &self.audio_rate.map(|r| r.to_string()).unwrap_or_default(),
            )
            .with_attr(
                "clock_priority",
                &self
                    .clock
                    .priority1
                    .map(|p| p.to_string())
                    .unwrap_or_default(),
            )
            .with_attr(
                "approve_programs",
                if self.approve_programs {
                    "true"
                } else {
                    "false"
                },
            )
            .with_attr(
                "link_requests",
                if self.link_requests { "true" } else { "false" },
            )
            .with_attr("programs", &rules_text(&self.programs))
            .with_attr("panel_pin", self.panel_pin.as_deref().unwrap_or(""))
            .with_attr(
                "panel_site_only",
                if self.panel_site_only {
                    "true"
                } else {
                    "false"
                },
            )
            .with_attr("site_lock", if self.site_lock { "true" } else { "false" });
        let mut e = e;
        let v = self.video.to_element();
        for k in idn_open_protocol::node::video::KEYS {
            e = e.with_attr(&format!("video_{k}"), v.attr(k).unwrap_or(""));
        }
        let a = self.audio.to_element();
        for k in idn_open_protocol::node::bridges::AUDIO_KEYS {
            e = e.with_attr(&format!("audio_{k}"), a.attr(k).unwrap_or(""));
        }
        let c = self.capture.to_element();
        for k in ["screens", "webcams"] {
            e = e.with_attr(&format!("capture_{k}"), c.attr(k).unwrap_or(""));
        }
        let l = self.sitelink.to_element();
        for k in idn_open_protocol::node::bridges::SITELINK_KEYS {
            e = e.with_attr(&format!("sitelink_{k}"), l.attr(k).unwrap_or(""));
        }
        let a = self.aes67.to_element();
        for k in idn_open_protocol::node::bridges::AES67_KEYS {
            e = e.with_attr(&format!("aes67_{k}"), a.attr(k).unwrap_or(""));
        }
        let l = self.listen.to_element();
        for k in idn_open_protocol::node::bridges::LISTEN_KEYS {
            e = e.with_attr(&format!("listen_{k}"), l.attr(k).unwrap_or(""));
        }
        let d = self.data.to_element();
        for k in idn_open_protocol::node::bridges::DATA_KEYS {
            e = e.with_attr(&format!("data_{k}"), d.attr(k).unwrap_or(""));
        }
        let o = self.omt.to_element();
        for k in idn_open_protocol::node::bridges::OMT_KEYS {
            e = e.with_attr(&format!("omt_{k}"), o.attr(k).unwrap_or(""));
        }
        let c = self.captions.to_element();
        for k in idn_open_protocol::node::bridges::CAPTIONS_KEYS {
            e = e.with_attr(&format!("captions_{k}"), c.attr(k).unwrap_or(""));
        }
        let n = self.netaudio.to_element();
        for k in idn_open_protocol::node::bridges::NETAUDIO_KEYS {
            e = e.with_attr(&format!("netaudio_{k}"), n.attr(k).unwrap_or(""));
        }
        e = e.with_attr("routers_list", &self.routers.text());
        let m = self.sitemanager.to_element();
        for k in idn_open_protocol::node::bridges::SITE_MANAGER_KEYS {
            e = e.with_attr(&format!("sitemanager_{k}"), m.attr(k).unwrap_or(""));
        }
        let a = self.apps.to_element();
        for k in idn_open_protocol::node::bridges::APPS_KEYS {
            e = e.with_attr(&format!("apps_{k}"), a.attr(k).unwrap_or(""));
        }
        e = e.with_attr("services_off", &self.services_off.text());
        if let Some(m) = &self.update_managed_by {
            e = e.with_attr("update_managed_by", m);
        }
        if !self.keep_awake {
            e = e.with_attr("keep_awake", "false");
        }
        write_private(path, e.to_xml().as_bytes())
    }
}
