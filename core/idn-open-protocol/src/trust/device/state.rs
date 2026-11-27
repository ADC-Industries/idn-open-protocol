//! The membership's state, identity and stored credentials; options and the CRL as they come.

use super::*;

impl Enrollment {
    /// The current state.
    pub fn state(&self) -> TrustState {
        self.state.lock_ok().clone()
    }

    /// The next state change, waiting up to `timeout`.
    pub fn recv(&self, timeout: Duration) -> Option<TrustState> {
        let (q, cv) = &self.events;
        let mut q = q.lock_ok();
        if q.is_empty() {
            q = cv
                .wait_timeout(q, timeout)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        q.pop_front()
    }

    /// Asks the authority again now instead of at the next retry (a "Try again" button).
    /// Useful when denied or revoked and the operator has cleared this device, or when
    /// waiting for the pairing window. Devices retry by themselves anyway: every minute
    /// when denied or revoked, every few seconds while waiting for pairing.
    pub fn retry_now(&self) {
        if let Some(h) = &self.node {
            h.send(Element::new("retry"));
        }
        self.kick.store(true, Ordering::Relaxed);
    }

    /// This device's key fingerprint, as the authority lists it.
    pub fn fingerprint(&self) -> String {
        self.key.fingerprint()
    }

    /// The `link/1` mode `open` identity made from this device's key (§E.11.4).
    pub(crate) fn open_identity(&self) -> Option<Arc<crate::link::Identity>> {
        if let Some(i) = self.open_identity.get() {
            return Some(i.clone());
        }
        let i = crate::link::Identity::from_key(self.key.clone()).ok()?;
        Some(self.open_identity.get_or_init(|| i).clone())
    }

    /// The device's name, as enrolled.
    pub(crate) fn name(&self) -> &str {
        &self.cfg.name
    }

    /// The Discovery Server hosting the authority (`host` or `host:port`).
    pub(crate) fn discovery_server(&self) -> &str {
        &self.cfg.discovery_server
    }

    /// Sends on the trust session; whether it went (no session: not).
    pub(super) fn trust_send(&self, xml: &str) -> bool {
        match self.trust_writer.lock_ok().as_mut() {
            Some(w) => proto::send(w, xml).is_ok(),
            None => false,
        }
    }

    #[doc(hidden)]
    pub fn site_key(&self) -> Option<Arc<SiteKey>> {
        self.site.read_ok().clone()
    }

    /// A newer options document replaces the one held (§E.20.4).
    pub(super) fn apply_options(&self, m: &Element) {
        if let Some(o) = super::super::options::SiteOptions::from_element(m) {
            let ca = self
                .creds()
                .map(|c| c.anchor.ca_fp.clone())
                .unwrap_or_default();
            let mut cur = self.options.lock_ok();
            let mut from = self.options_ca.lock_ok();
            // Another site's (or of a site not known before): taken whatever its version.
            let new_site = !ca.is_empty() && *from != ca;
            if o.version > cur.version || new_site {
                if self.node.is_none() {
                    // (Whole: the authority sends them again at each connection anyway.)
                    let _ = idn_common::fsutil::write(self.path("options.xml"), o.to_xml());
                    if new_site {
                        let _ = idn_common::fsutil::write(self.path("options.ca"), ca.as_bytes());
                    }
                }
                if new_site {
                    *from = ca;
                }
                *cur = Arc::new(o);
                self.options_cv.notify_all();
            }
        }
    }

    /// A new CRL: our own serial in it means revoked; sessions with peers revoked now are
    /// closed (§E.10.6). Returns false when this device itself is revoked.
    pub(super) fn apply_crl(&self, creds: &DeviceCreds, der: &[u8]) -> io::Result<bool> {
        if let Some(crl) = creds.anchor.accept_crl(der)? {
            if self.node.is_none() {
                let _ = super::super::pki::write_private(&self.path("crl.der"), der);
            }
            if creds
                .anchor
                .verify_with(&creds.cert_der, false)
                .is_ok_and(|me| crl.serials.contains(&me.serial))
            {
                self.forget_creds();
                self.set(TrustState::Revoked);
                return Ok(false);
            }
            self.sessions.lock_ok().retain(|(serial, sock)| {
                if crl.serials.contains(serial) {
                    let _ = sock.shutdown(std::net::Shutdown::Both);
                    false
                } else {
                    true
                }
            });
        }
        Ok(true)
    }

    /// TLS credentials, once enrolled.
    #[doc(hidden)]
    pub fn creds(&self) -> Option<Arc<DeviceCreds>> {
        self.creds.read_ok().clone()
    }

    /// This device's name in the site, once enrolled: its certificate's, which is the
    /// name it asked with or the one the operator gave as it was approved
    /// ([`Authority::approve_as`](crate::trust::Authority::approve_as)).
    pub fn name_in_site(&self) -> Option<String> {
        let c = self.creds()?;
        c.anchor
            .verify_with(&c.cert_der, false)
            .ok()
            .map(|i| i.name)
    }

    /// The first 16 hex digits of the site CA's fingerprint, once enrolled (`x_idn_ca`).
    pub fn ca_id(&self) -> Option<String> {
        self.creds().map(|c| c.anchor.ca_id().to_owned())
    }

    pub(super) fn set(&self, st: TrustState) {
        let mut cur = self.state.lock_ok();
        if *cur != st {
            *cur = st.clone();
            let (q, cv) = &self.events;
            q.lock_ok().push_back(st);
            cv.notify_all();
        }
    }

    pub(super) fn path(&self, f: &str) -> PathBuf {
        self.cfg.state_dir.join(f)
    }

    pub(super) fn load_creds(&self) {
        let (Ok(cert), Ok(ca)) = (
            std::fs::read(self.path("device.crt")),
            std::fs::read(self.path("ca.crt")),
        ) else {
            return;
        };
        let Ok(anchor) = Anchor::new(ca) else { return };
        if let Ok(crl) = std::fs::read(self.path("crl.der")) {
            let _ = anchor.accept_crl(&crl);
        }
        let key = match KeyFile::load_or_create(&self.path("device.key")) {
            Ok(k) => k,
            Err(_) => return,
        };
        let creds = Arc::new(DeviceCreds {
            key,
            cert_der: cert,
            anchor: Arc::new(anchor),
        });
        self.load_lan_cert(&creds);
        *self.creds.write_ok() = Some(creds);
    }

    pub(super) fn forget_creds(&self) {
        *self.creds.write_ok() = None;
        self.forget_lan_cert();
        if self.node.is_none() {
            let _ = std::fs::remove_file(self.path("device.crt"));
        }
    }

    /// Nobody but the background thread holds this enrollment any more, or it was told
    /// to stop: its waits end (TR-4, R1-114).
    pub(super) fn abandoned(&self) -> bool {
        self.stop.load(Ordering::Relaxed) || self.me.get().is_some_and(|w| w.strong_count() <= 1)
    }
}
