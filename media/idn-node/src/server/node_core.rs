//! The Node itself: starting it, its accessors, and joining and leaving a site.

use super::*;

impl NodeCore {
    /// Starts the Node with its state in `dir` (created, readable by this account only).
    pub fn start(dir: &Path) -> io::Result<Arc<NodeCore>> {
        std::fs::create_dir_all(dir)?;
        let _ = idn_common::fsutil::private_dir(dir);
        let settings = Settings::load(&dir.join("node.xml"));
        let device_key = KeyFile::load_or_create(&dir.join("device.key"))?;
        let device_open = Identity::from_key(device_key.clone())?;
        let link = LinkConfig::in_dir(dir)?;
        let (events, _) = tokio::sync::broadcast::channel(256);
        // Typed events (§N.41): to the Site Manager as alerts carrying them, once a minute
        // at most per event and subject.
        let alerts: Arc<Mutex<VecDeque<Alert>>> = Arc::default();
        let problem_events = idn_open_protocol::web::problems::Events::new();
        {
            let q = alerts.clone();
            problem_events.log.set_sink(move |l| {
                let app = l.detail["app"].as_str().unwrap_or("idn-node").to_owned();
                queue_alert(
                    &q,
                    Alert {
                        at: idn_common::epoch::secs(),
                        app,
                        level: l.level.clone(),
                        text: l.text.clone(),
                        event: l.event.clone(),
                        detail: l.detail.to_string(),
                    },
                );
            });
        }
        let core = Arc::new(NodeCore {
            dir: dir.to_owned(),
            settings: Mutex::new(settings),
            changing: Mutex::new(()),
            device_key,
            device_open,
            link,
            member: RwLock::new(Member::default()),
            clients: Mutex::new(BTreeMap::new()),
            next: AtomicU64::new(1),
            events,
            log: Mutex::new(VecDeque::new()),
            tokens: Mutex::new(Vec::new()),
            data_panel: idn_open_protocol::web::HandOff::random(),
            stop: AtomicBool::new(false),
            started: Instant::now(),
            sites: Mutex::new(Vec::new()),
            floors: Mutex::default(),
            levels_told: Mutex::default(),
            key_tickets: Mutex::default(),
            service_changes: Default::default(),
            routes: Mutex::new(RouteTable::load(&dir.join("routes.xml"))),
            clock: Mutex::new((None, (0, 0), None)),
            offsets: Mutex::default(),
            node_services: Mutex::new(BTreeMap::new()),
            alerts: alerts.clone(),
            problem_events,
            problems_now: Mutex::default(),
            audio_apps: Mutex::default(),
            trial_site: Mutex::default(),
            vbcable_request: Mutex::default(),
            media: crate::media::Media::new(crate::media::media_dir(dir, false)),
            pin_lock: idn_open_protocol::web::PinLock::default(),
            search_now: AtomicBool::new(false),
            update: Mutex::default(),
            update_available: Mutex::default(),
            service: AtomicBool::new(false),
            pairs: crate::pairs::Pairs::load(dir.join("pairs.xml")),
            me: std::sync::OnceLock::new(),
        });
        let _ = core.me.set(Arc::downgrade(&core));
        core.listen_for_links();
        core.log(&format!(
            "IDN Node {} ({}) starting; device {} ({}…)",
            env!("CARGO_PKG_VERSION"),
            idn_open_protocol::build_line(),
            core.settings().name,
            &core.device_key.fingerprint()[..16]
        ));
        core.rebuild_member(None);
        core.apply_library();
        core.restart_clock();
        let weak = Arc::downgrade(&core);
        std::thread::Builder::new()
            .name("idn-node-watch".into())
            .spawn(move || watch(weak))?;
        let weak = Arc::downgrade(&core);
        std::thread::Builder::new()
            .name("idn-node-site".into())
            .spawn(move || site_loop(weak))?;
        Ok(core)
    }

    /// Stops the background work (the service is ending).
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.media.stop();
        self.pairs.stop();
        self.clock.lock_ok().0 = None;
        *self.member.write_ok() = Member::default();
    }

    /// The state directory (keys, settings, the panel's certificate).
    pub fn state_dir(&self) -> &Path {
        &self.dir
    }

    /// The Site Manager's folder when this Node runs it (§N.12). On Windows beside the
    /// Node's state, not in it: removing the machine's identity (the uninstaller's
    /// question) deletes that folder, and mustn't take the site's keys with it.
    pub fn site_manager_dir(&self) -> PathBuf {
        if cfg!(windows) {
            self.dir.with_file_name("IDN Site Manager")
        } else {
            self.dir.join("site-manager")
        }
    }

    /// [`NodeCore::site_manager_dir`], made if need be: private to the account it runs as
    /// (on Windows, beside the Node's state, it would take ProgramData's access, which lets
    /// users read; SYSTEM and administrators only, as the Node's own folder).
    ///
    /// A link or junction there (the folder on another disk) is followed; a folder another
    /// account owns, or one that couldn't be made private, is logged and used as it is
    /// (it was an error before, and the Site Manager didn't start).
    pub(super) fn make_site_manager_dir(&self) -> io::Result<PathBuf> {
        let dir = self.site_manager_dir();
        match idn_common::fsutil::private_dir(&dir) {
            Ok(true) => {}
            Ok(false) => self.log(&format!(
                "site manager: {}: another account's folder: its access left as it is",
                dir.display()
            )),
            Err(e) => {
                self.log(&format!(
                    "site manager: {}: not made private ({e}); used as it is",
                    dir.display()
                ));
                std::fs::create_dir_all(&dir)?;
            }
        }
        // Every time on Windows, owner first: a folder someone else made before (in
        // ProgramData, where standard users may) stayed theirs.
        #[cfg(windows)]
        if let Err(e) = idn_common::fsutil::lock_down_dir(&dir) {
            self.log(&format!("site manager: {e}"));
        }
        Ok(dir)
    }

    /// The current settings.
    pub fn settings(&self) -> Settings {
        self.settings.lock_ok().clone()
    }

    /// The machine's sample rate (`node::rate`, §N.40): its own setting, else the site
    /// option `audio.rate` for this machine, else 48 kHz; and where it came from.
    pub fn audio_rate(&self) -> (u32, idn_open_protocol::node::rate::Source) {
        let site = self.enrollment().and_then(|e| {
            e.options()
                .for_device(&self.fingerprint())
                .get("audio.rate")
                .map(str::to_owned)
        });
        let r = idn_open_protocol::node::rate::resolve(self.settings().audio_rate, site.as_deref());
        MACHINE_RATE.store(r.0, std::sync::atomic::Ordering::Relaxed);
        r
    }

    /// The device's key fingerprint.
    pub fn fingerprint(&self) -> String {
        self.device_key.fingerprint()
    }

    /// The enrollment, when a server is set.
    pub fn enrollment(&self) -> Option<Arc<Enrollment>> {
        self.member.read_ok().enrollment.clone()
    }

    /// The media folder as the site's library or not, from the settings (§N.26).
    pub(super) fn apply_library(&self) {
        let s = self.settings();
        self.media.set_library(
            s.data.library,
            &s.name,
            s.interface.as_deref().unwrap_or(""),
        );
    }

    /// The connected programs.
    pub fn clients(&self) -> Vec<Client> {
        self.clients.lock_ok().values().cloned().collect()
    }

    /// How long the Node has run.
    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    /// Whether this machine holds a site certificate: then it neither links nor uses its
    /// links (§N.32).
    pub(super) fn in_site(&self) -> bool {
        self.member
            .read_ok()
            .enrollment
            .as_ref()
            .is_some_and(|e| e.creds().is_some())
    }

    /// Signs a TLS handshake for a program (§N.4.4).
    pub(super) fn sign(&self, key: &str, data: &[u8]) -> Result<Vec<u8>, (&'static str, String)> {
        if !is_certificate_verify_input(data) {
            return Err((
                "denied",
                "only TLS 1.3 CertificateVerify inputs are signed".into(),
            ));
        }
        let r = match key {
            "device" => self.device_key.sign(data),
            "link" => self.link.identity.sign(data),
            "site" => match self.member.read_ok().site.clone() {
                Some(k) => k.sign(data),
                None => return Err(("state", "no passphrase set".into())),
            },
            _ => return Err(("bad", format!("no key {key:?}"))),
        };
        r.map_err(|e| ("failed", e.to_string()))
    }

    /// The Site Managers found on the LAN (§N.11.4), while no server is set.
    pub fn sites_found(&self) -> Vec<idn_open_protocol::discovery::site::SiteInfo> {
        self.sites.lock_ok().clone()
    }

    /// Locked by the site (N4, §N.31), as it holds now: only while the machine belongs to
    /// a site that hasn't revoked or denied it. A machine the site dropped, or one whose
    /// site is gone (its server unset), is the machine's own again; else nothing on it
    /// could ever unlock it.
    pub fn site_locked(&self) -> bool {
        let s = self.settings();
        if !s.site_lock || s.server.is_none() {
            return false;
        }
        let m = self.member.read_ok();
        !matches!(
            m.enrollment.as_ref().map(|e| e.state()),
            Some(TrustState::Revoked | TrustState::Denied | TrustState::NoAuthority)
        )
    }

    /// A routing matrix watches this machine's levels (idn-routing.md §A.15): the programs
    /// with outputs are told to report them (for 5 s, told again every 2 s while it
    /// watches).
    pub fn watch_levels(&self) {
        {
            let mut t = self.levels_told.lock_ok();
            if t.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
                return;
            }
            *t = Some(Instant::now());
        }
        let cids: Vec<u64> = self
            .clients
            .lock_ok()
            .iter()
            .filter(|(_, c)| c.regs.values().any(|r| r.kind == "output"))
            .map(|(cid, _)| *cid)
            .collect();
        let e = Element::new("meters").with_attr("secs", "5");
        for cid in cids {
            self.tell(cid, &e);
        }
    }

    /// Whether a routing matrix asked for levels in the last 5 s (one is open).
    pub(crate) fn matrix_watching(&self) -> bool {
        self.levels_told
            .lock_ok()
            .is_some_and(|t| t.elapsed() < Duration::from_secs(5))
    }

    /// The levels of this machine's transmitters, from the last second (§A.15):
    /// `<levels><tx name db/>…</levels>`; asking counts as watching ([`watch_levels`]).
    pub fn levels_element(&self) -> Element {
        self.watch_levels();
        let mut e = Element::new("levels");
        for c in self.clients.lock_ok().values() {
            for r in c.regs.values().filter(|r| r.kind == "output") {
                if let Some((at, db)) = &r.levels
                    && at.elapsed() < Duration::from_secs(1)
                {
                    e = e.with_child(
                        Element::new("tx")
                            .with_attr("name", &r.name)
                            .with_attr("db", db),
                    );
                }
            }
        }
        e
    }

    /// Sends `e` to one program.
    pub(super) fn tell(&self, cid: u64, e: &Element) {
        if let Some(out) = self.clients.lock_ok().get(&cid).and_then(|c| c.out.clone()) {
            out.send(e.to_xml());
        }
    }

    /// How the services the Node service runs itself (the site link, AES67) are doing,
    /// by id: from the supervisor running them.
    pub fn set_node_services(&self, states: BTreeMap<String, BridgeState>) {
        *self.node_services.lock_ok() = states;
    }

    /// The panel port for this run only (a command-line override).
    pub fn set_panel_port(&self, port: u16) {
        let mut s = self.settings.lock_ok();
        let now = (s.panel_port, s.lan_port);
        s.saved_ports.get_or_insert(now);
        s.panel_port = port;
    }

    /// The LAN panel port for this run only (a command-line override).
    pub fn set_lan_port(&self, port: u16) {
        let mut s = self.settings.lock_ok();
        let now = (s.panel_port, s.lan_port);
        s.saved_ports.get_or_insert(now);
        s.lan_port = port;
    }

    /// "Try again now" (§N.4.2).
    pub fn retry(&self) {
        if let Some(e) = self.enrollment() {
            e.retry_now();
        }
    }

    /// Joins the site with a code its Site Manager issued (idn-node.md §N.11.8,
    /// extensions §E.10.10): `"482 913"` or `"482913"`, used once, at once.
    pub fn join_code(&self, code: &str) -> Result<(), String> {
        let e = self
            .enrollment()
            .ok_or("no site to join: set the site's address first")?;
        // Already in a site (it holds the site's certificate): a typed refusal (§N.41),
        // the machine left first.
        if matches!(
            e.state(),
            idn_open_protocol::trust::TrustState::Enrolled { .. }
        ) {
            let site = self.settings().server.unwrap_or_default();
            let text = format!(
                "this machine is already in a site ({site}): leave it first to join another"
            );
            self.problem_events.emit(
                "already_in_site",
                "warning",
                &text,
                serde_json::json!({"server": site}),
            );
            return Err(text);
        }
        e.join_with_code(code).map_err(|e| e.to_string())?;
        self.log("site: a join code typed; asking the site with it");
        Ok(())
    }

    /// Leaves the site (idn-node.md §N.42): no site address, no search for one, and the
    /// site's certificates on this machine forgotten (`device.crt`, `ca.crt`, `crl.der`,
    /// `lan.crt`); the membership started again with none. The site is told first, while
    /// connected (`idn_leave`): it drops the machine from its list (it may join again).
    /// Refused on the machine that runs the site (its Site Manager): that is the site.
    pub fn leave_site(&self) -> Result<(), String> {
        // A machine its site locked (N4, §N.31) is let go by its Site Manager, not here.
        if self.site_locked() {
            return Err("this machine is locked by its site: its Site Manager removes it".into());
        }
        {
            let mut held = self.settings.lock_ok();
            if held.server.as_deref() == Some(site_here::OWN_SERVER) && held.sitemanager.on {
                return Err(
                    "this machine runs the site (its Site Manager): turn that off first".into(),
                );
            }
            let mut s = held.clone();
            s.server = None;
            s.auto_site = false;
            s.save(&self.dir.join("node.xml"))
                .map_err(|e| format!("saving: {e}"))?;
            *held = s;
        }
        *self.trial_site.lock_ok() = None;
        if let Some(e) = self.enrollment() {
            let _ = e.tell_leaving();
        }
        // (The site's options too: a machine waiting to join another site kept the old
        // one's in force; bug run 2026-11-25.)
        for f in [
            "device.crt",
            "ca.crt",
            "crl.der",
            "lan.crt",
            "options.xml",
            "options.ca",
        ] {
            let _ = std::fs::remove_file(self.dir.join(f));
        }
        self.rebuild_member(None);
        self.log("site: left the site (its certificates forgotten)");
        let _ = self.events.send(self.identity().to_xml());
        Ok(())
    }

    /// A one-time panel token (§N.6): random, valid for 60 s. Not `full`: its session
    /// can't change who may use the Node, its LAN password, site or passphrase (a program
    /// waiting for approval ran `idn-node open` and approved itself; bug round F, F6-1).
    pub fn issue_token(&self, full: bool) -> String {
        let t = idn_open_protocol::random::token_hex(16);
        let mut v = self.tokens.lock_ok();
        v.retain(|(_, at, _)| at.elapsed() < Duration::from_secs(60));
        v.push((t.clone(), Instant::now(), full));
        t
    }

    /// IDN Data's panel key, for its environment.
    pub fn data_panel_key(&self) -> String {
        self.data_panel.key_hex()
    }

    /// A one-time token opening IDN Data's panel.
    pub fn data_panel_token(&self) -> String {
        self.data_panel.issue()
    }

    /// This Node runs as the system's service (not by hand): an update stops and starts the
    /// service.
    pub fn set_service(&self, v: bool) {
        self.service.store(v, Ordering::Relaxed);
        self.media.set_dir(crate::media::media_dir(&self.dir, v));
    }

    /// Where an update is (§N.23), for the report and the panel.
    /// Asks the site for its newest release for this machine (its product, its track),
    /// now (idn-node.md §N.42): the answer arrives as `update.available` in the status.
    pub fn check_updates(&self) -> Result<(), String> {
        let e = self.enrollment().ok_or("not in a site")?;
        e.send_to_authority(
            &Element::new("idn_update_check")
                .with_attr("product", idn_open_protocol::release::MEDIA)
                .with_attr("version", &idn_open_protocol::build_line()),
        );
        self.log("updates: asked the site for its newest release");
        Ok(())
    }

    /// The site's answer to [`NodeCore::check_updates`].
    pub(crate) fn take_update_available(&self, version: &str) {
        *self.update_available.lock_ok() = Some((version.to_owned(), idn_common::epoch::secs()));
        let _ = self.events.send(self.identity().to_xml());
    }

    /// `<update …/>` for the status: an update in progress (state, version, progress,
    /// error), the site's newest release as last asked (`available`, `checked_at`), and
    /// who manages updates when another product does (`managed_by`, `managed_by_source`).
    pub(crate) fn update_element(&self) -> Element {
        let mut e = self
            .update_progress()
            .map(|u| crate::update::ProgressElement::element(&u))
            .unwrap_or_else(|| Element::new("update"));
        if let Some((v, at)) = self.update_available.lock_ok().clone() {
            e = e
                .with_attr("available", &v)
                .with_attr("checked_at", &at.to_string());
        }
        // Managed by another product (§N.23): who, and whether its package or the
        // setting says so.
        crate::update::with_managed(e, self.update_managed_by().as_ref())
    }

    /// Who installs and updates this machine's IDN Node, if another product does (the
    /// marker in the programs folder, else the setting; §N.23).
    pub(crate) fn update_managed_by(&self) -> Option<crate::update::ManagedBy> {
        let setting = self.settings.lock_ok().update_managed_by.clone();
        crate::update::managed_by(setting.as_deref())
    }

    pub fn update_progress(&self) -> Option<crate::update::Progress> {
        self.update
            .lock_ok()
            .clone()
            .or_else(|| crate::update::from_files(&self.dir))
    }

    /// Looks for a Site Manager on the network now (with no site set and finding on; a
    /// single one found is joined, as always: §N.11.4).
    pub fn search_sites_now(&self) {
        self.search_now.store(true, Ordering::Relaxed);
    }
}
