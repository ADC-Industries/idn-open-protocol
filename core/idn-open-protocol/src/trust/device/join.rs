//! Starting a membership and joining: typed codes, confirmation, the join loop's thread.

use super::*;

impl Enrollment {
    /// Loads (or creates) this device's identity and starts talking to the authority in
    /// the background. Fails only if the state directory can't be used or the server
    /// address doesn't resolve.
    pub fn start(cfg: EnrollmentConfig) -> io::Result<Arc<Self>> {
        let site = cfg
            .passphrase
            .as_deref()
            .filter(|p| !p.is_empty())
            .map(SiteKey::from_passphrase)
            .transpose()?;
        Self::start_with_site_key(cfg, site)
    }

    /// As [`Enrollment::start`] with the passphrase's key already derived (the IDN Node
    /// keeps the key, not the passphrase); `cfg.passphrase` is ignored.
    #[doc(hidden)]
    pub fn start_with_site_key(
        cfg: EnrollmentConfig,
        site: Option<Arc<SiteKey>>,
    ) -> io::Result<Arc<Self>> {
        Self::start_inner(cfg, site, false)
    }

    /// As [`Enrollment::start_with_site_key`], with [`Enrollment::confirm_codes`] on from
    /// the start: set after, the join's thread could take a quick approval before it was
    /// (bug round F, F6-8).
    pub fn start_confirming(
        cfg: EnrollmentConfig,
        site: Option<Arc<SiteKey>>,
    ) -> io::Result<Arc<Self>> {
        Self::start_inner(cfg, site, true)
    }

    fn start_inner(
        cfg: EnrollmentConfig,
        site: Option<Arc<SiteKey>>,
        confirm: bool,
    ) -> io::Result<Arc<Self>> {
        std::fs::create_dir_all(&cfg.state_dir)?;
        let key = KeyFile::load_or_create(&cfg.state_dir.join("device.key"))?;
        let server = crate::discovery::ds::server_addr(&cfg.discovery_server)?;
        let options_path = cfg.state_dir.join("options.xml");
        let e = Arc::new(Enrollment {
            node: None,
            node_interface: None,
            open_identity: std::sync::OnceLock::new(),
            endpoint: Mutex::new(None),
            directory: Mutex::new(None),
            site_devices: Mutex::new(None),
            site_txs: Mutex::new((Vec::new(), None)),
            trust_writer: Mutex::new(None),
            cfg,
            key,
            site: RwLock::new(site),
            creds: RwLock::new(None),
            state: Mutex::new(TrustState::Starting),
            events: Default::default(),
            sessions: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            me: Default::default(),
            options: Mutex::new(Arc::new(
                // (One that can't be read is set aside and said; the site sends its
                // options again once connected.)
                idn_common::fsutil::load_saved(&options_path, |b| {
                    super::super::options::SiteOptions::from_xml(std::str::from_utf8(b).ok()?)
                })
                .unwrap_or_else(|why| {
                    crate::log::log(&format!("site options: {why}"));
                    None
                })
                .unwrap_or_default(),
            )),
            options_ca: Mutex::new(
                std::fs::read_to_string(options_path.with_extension("ca"))
                    .map(|t| t.trim().to_owned())
                    .unwrap_or_default(),
            ),
            options_cv: Condvar::new(),
            admin_code_refused: AtomicBool::new(false),
            admin_replies: Default::default(),
            admin_next: Default::default(),
            key_answers: Mutex::new((1, Default::default())),
            pairing_codes: Default::default(),
            key_code_wrong: Default::default(),
            kick: AtomicBool::new(false),
            node_require: AtomicBool::new(false),
            node_sets: Default::default(),
            pairs: Default::default(),
            pair_uses: Default::default(),
            pair_sessions: Default::default(),
            confirm_codes: AtomicBool::new(confirm),
            confirmation: Default::default(),
            typed_code: Mutex::new(None),
            code_in_use: Mutex::new(None),
            site_mode: Mutex::new(None),
            lan: Default::default(),
        });
        let _ = e.me.set(Arc::downgrade(&e));
        e.load_creds();
        let weak = Arc::downgrade(&e);
        std::thread::Builder::new()
            .name("idn-trust".into())
            .spawn(move || run(weak, server))?;
        Ok(e)
    }

    /// Joins by code wait for this device's person to confirm the code
    /// ([`TrustState::ConfirmCode`], [`confirm_code`](Self::confirm_code)) before the site is
    /// trusted: the IDN Node turns it on for its machine.
    pub fn confirm_codes(&self, on: bool) {
        self.confirm_codes.store(on, Ordering::Relaxed);
    }

    /// This device's person's answer: the authority shows the same code (`matches`), or
    /// not (the join stops).
    pub fn confirm_code(&self, matches: bool) {
        *self.confirmation.0.lock_ok() = Some(matches);
        self.confirmation.1.notify_all();
    }

    /// Joins with a code the site's Site Manager issued (§E.10.10): `"482 913"` or
    /// `"482913"`. Used for the next join, at once (a join waiting for approval gives way
    /// to it); once, whatever the answer. Refused when it isn't six digits, when this
    /// membership is the IDN Node's (type it there), or once enrolled.
    pub fn join_with_code(&self, code: &str) -> io::Result<()> {
        let code = super::super::join_code::normalize(code).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "a join code is six digits")
        })?;
        if self.node.is_some() {
            return Err(err(
                "this membership is the IDN Node's: type the code there",
            ));
        }
        if self.creds().is_some() {
            return Err(err("already in the site"));
        }
        *self.typed_code.lock_ok() = Some(code);
        // Until the site answers: no earlier answer shown as this code's.
        self.set(TrustState::Starting);
        self.retry_now();
        Ok(())
    }

    /// The site's join mode, as its authority said when last asked (`None`: not known
    /// yet).
    pub fn site_join_mode(&self) -> Option<super::super::JoinMode> {
        *self.site_mode.lock_ok()
    }

    /// One join. The site's answer uses the typed code up (each try costs one of its
    /// five); a connection lost before one keeps it for the next try (it was
    /// taken at the start, a join cut short under load left the device waiting for
    /// approval with the code gone).
    fn join_once(&self, server: SocketAddr) -> io::Result<JoinOutcome> {
        let r = self.join(server);
        let used = self.code_in_use.lock_ok().take();
        if r.as_ref()
            .is_ok_and(|o| !matches!(o, JoinOutcome::NoAuthority))
        {
            let mut t = self.typed_code.lock_ok();
            if *t == used {
                *t = None;
            }
        }
        r
    }
}

/// How often a denied or revoked device asks again (so that clearing it on the authority
/// takes effect without restarting it: within 15 s).
const REFUSED_RETRY: Duration = Duration::from_secs(15);

fn run(weak: std::sync::Weak<Enrollment>, server: SocketAddr) {
    loop {
        let Some(e) = weak.upgrade() else { return };
        if e.stop.load(Ordering::Relaxed) {
            return;
        }
        let wait = match e.creds() {
            Some(creds) => {
                let r = e.session(server, creds);
                // Not connected from now (before the refusals below: a request made in
                // between is refused at once rather than left waiting).
                if e.state() != TrustState::Revoked {
                    e.set(TrustState::Enrolled { connected: false });
                }
                // Pairings relayed on that connection get no answer now: refused, so the
                // client can ask again (the site ended them as the connection closed).
                e.fail_waiting_keys("the connection to the site was lost: pair again");
                if e.state() == TrustState::Revoked {
                    // Ask again later (refused until the operator clears this device).
                    drop(e);
                    if !pause(&weak, REFUSED_RETRY) {
                        return;
                    }
                    continue;
                }
                e.set(TrustState::Enrolled { connected: false });
                if let Err(x) = r {
                    crate::log::log(&format!("trust: authority: {x}"));
                }
                Duration::from_secs(2)
            }
            None => match e.join_once(server) {
                Ok(JoinOutcome::Enrolled) => Duration::ZERO,
                Ok(JoinOutcome::Closed) => {
                    e.set(TrustState::WaitingForPairing);
                    Duration::from_secs(3)
                }
                Ok(JoinOutcome::Denied) => {
                    // A revoked device stays "Revoked" while refused; anything else "Denied".
                    if e.state() != TrustState::Revoked {
                        e.set(TrustState::Denied);
                    }
                    REFUSED_RETRY
                }
                Ok(JoinOutcome::NoAuthority) => {
                    e.set(TrustState::NoAuthority);
                    Duration::from_secs(30)
                }
                // The join ends (idn-node.md §N.11): asked for again only by the person
                // (Try again, `retry_now`).
                Ok(JoinOutcome::Unconfirmed) => {
                    e.set(TrustState::Error(
                        "the site's code wasn't confirmed on this computer: not joined (Try again to ask anew)".into(),
                    ));
                    Duration::from_secs(365 * 24 * 3600)
                }
                // The typed code's answers (§E.10.10): shown until the person types
                // another code or asks again (Try again), never tried again by itself (each
                // try costs one of the code's five).
                Ok(JoinOutcome::WrongCode(left)) => {
                    e.set(TrustState::WrongCode { left });
                    Duration::from_secs(365 * 24 * 3600)
                }
                Ok(JoinOutcome::CodeExpired) => {
                    e.set(TrustState::CodeExpired);
                    Duration::from_secs(365 * 24 * 3600)
                }
                // The site's mode may change: asked again now and then.
                Ok(JoinOutcome::CodeNeeded) => {
                    e.set(TrustState::CodeNeeded);
                    REFUSED_RETRY
                }
                Ok(JoinOutcome::CodeLimited(secs)) => {
                    e.set(TrustState::Error(format!(
                        "too many wrong join codes from this computer: wait {} minutes",
                        secs.div_ceil(60)
                    )));
                    Duration::from_secs(secs.max(1))
                }
                // Another code typed: the join with it at once.
                Err(x) if x.get_ref().is_some_and(|i| i.is::<CodeTyped>()) => Duration::ZERO,
                Err(x) => {
                    e.set(TrustState::Error(x.to_string()));
                    Duration::from_secs(5)
                }
            },
        };
        drop(e);
        if !pause(&weak, wait) {
            return;
        }
    }
}
