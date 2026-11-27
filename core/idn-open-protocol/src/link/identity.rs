//! This device's link identity, the peer keys it remembers, its link settings, and the
//! names of link modes.

use super::*;

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Identity({})", &self.fingerprint()[..16]) // never print key material
    }
}

impl Identity {
    /// Loads the key at `path`, or creates it there (readable by this user only).
    pub fn load_or_create(path: &Path) -> io::Result<Arc<Self>> {
        Self::from_key(KeyFile::load_or_create(path)?)
    }

    /// A key for this process only: peers see a new key after every restart.
    pub fn ephemeral() -> io::Result<Arc<Self>> {
        Self::from_key(KeyFile::generate()?)
    }

    /// A key held by the IDN Node, with the certificate the Node made for it
    /// (spec/extensions/idn-node.md §N.4.3).
    pub(crate) fn remote(key: KeyFile, cert_der: Vec<u8>) -> Arc<Self> {
        Arc::new(Identity {
            key,
            cert: CertificateDer::from(cert_der),
        })
    }

    /// The public key.
    #[doc(hidden)]
    pub fn public(&self) -> [u8; 32] {
        self.key.public
    }

    /// The self-signed certificate for mode `open` (DER).
    #[doc(hidden)]
    pub fn cert_der(&self) -> &[u8] {
        &self.cert
    }

    /// Signs a TLS handshake message with this identity's key (the IDN Node, for programs).
    #[doc(hidden)]
    pub fn sign(&self, message: &[u8]) -> io::Result<Vec<u8>> {
        self.key.sign(message)
    }

    #[doc(hidden)]
    pub fn from_key(key: KeyFile) -> io::Result<Arc<Self>> {
        let kp = key.rcgen()?;
        let mut p = rcgen::CertificateParams::new(vec![SERVER_NAME.to_owned()])
            .map_err(io::Error::other)?;
        // Deterministic for the key (fixed serial and validity): the same certificate
        // each time it is built.
        p.serial_number = Some(rcgen::SerialNumber::from(1u64));
        p.not_before = rcgen::date_time_ymd(2000, 1, 1);
        p.not_after = rcgen::date_time_ymd(9999, 12, 31);
        p.distinguished_name = rcgen::DistinguishedName::new();
        p.distinguished_name
            .push(rcgen::DnType::CommonName, "IDN device");
        let cert = p.self_signed(&kp).map_err(io::Error::other)?.der().clone();
        Ok(Arc::new(Identity { key, cert }))
    }

    /// The key's fingerprint (§E.10.1): SHA-256 of the SPKI, 64 hex digits.
    pub fn fingerprint(&self) -> String {
        self.key.fingerprint()
    }

    /// The certificate with its key, for a TLS configuration's resolver (linked
    /// computers, §E.28).
    pub(crate) fn certified(
        &self,
        provider: &rustls::crypto::CryptoProvider,
    ) -> io::Result<Arc<rustls::sign::SingleCertAndKey>> {
        self.key.certified(vec![self.cert.clone()], provider)
    }
}

impl KeyMemory {
    /// Remembered for the life of the process only.
    pub fn in_memory() -> Arc<Self> {
        Arc::new(KeyMemory {
            path: None,
            keys: Default::default(),
        })
    }

    /// Remembered in `path` (lines of `fingerprint<TAB>peer name`).
    pub fn with_file(path: PathBuf) -> Arc<Self> {
        // (One that can't be read is set aside and said, not saved over with none.)
        let text = idn_common::fsutil::load_saved(&path, |b| String::from_utf8(b.to_vec()).ok())
            .unwrap_or_else(|why| {
                crate::log::log(&format!("linked computers: {why}"));
                None
            })
            .unwrap_or_default();
        let keys = text
            .lines()
            .filter_map(|l| {
                let (fp, name) = l.split_once('\t')?;
                Some((name.to_owned(), fp.to_owned()))
            })
            .collect();
        Arc::new(KeyMemory {
            path: Some(path),
            keys: Mutex::new(keys),
        })
    }

    /// Remembers `fingerprint` for `peer`. Returns true if a different key was
    /// remembered for it: warn the user.
    pub fn check(&self, peer: &str, fingerprint: &str) -> bool {
        let mut k = self.keys.lock_ok();
        let before = k.insert(Self::key(peer), fingerprint.to_owned());
        if before.as_deref() != Some(fingerprint) {
            self.save(&k);
        }
        before.is_some_and(|b| b != fingerprint)
    }

    /// Forgets `peer`'s key (the operator accepted the change).
    pub fn forget(&self, peer: &str) {
        let mut k = self.keys.lock_ok();
        if k.remove(&Self::key(peer)).is_some() {
            self.save(&k);
        }
    }

    /// A peer's name as kept (one form for `check` and `forget`: a name with a tab or line
    /// break could never be forgotten).
    fn key(peer: &str) -> String {
        peer.to_lowercase().replace(['\t', '\n', '\r'], " ")
    }

    fn save(&self, k: &BTreeMap<String, String>) {
        if let Some(p) = &self.path {
            let t: String = k.iter().map(|(n, f)| format!("{f}\t{n}\n")).collect();
            if let Err(e) = crate::trust::pki::write_private(p, t.as_bytes()) {
                crate::log::log(&format!("link: could not save {}: {e}", p.display()));
            }
        }
    }
}

impl LinkConfig {
    /// Key and key memory in `dir` (`link.key`, `link-peers.tsv`).
    pub fn in_dir(dir: &Path) -> io::Result<Self> {
        Ok(LinkConfig {
            identity: Identity::load_or_create(&dir.join("link.key"))?,
            keys: KeyMemory::with_file(dir.join("link-peers.tsv")),
        })
    }

    /// A key for this process only, and key memory in memory: for tests and tools.
    pub fn ephemeral() -> Self {
        LinkConfig {
            identity: Identity::ephemeral().expect("generating an Ed25519 key"),
            keys: KeyMemory::in_memory(),
        }
    }

    /// Where [`LinkConfig::default`] keeps its files: `$IDN_STATE_DIR`, else
    /// `$XDG_STATE_HOME/idn-open-protocol`, else `~/.local/state/idn-open-protocol`.
    pub fn default_dir() -> Option<PathBuf> {
        idn_common::paths::state_dir()
    }
}

impl Default for LinkConfig {
    /// Persistent in [`LinkConfig::default_dir`]; for this process only if that can't
    /// be used.
    fn default() -> Self {
        LinkConfig::default_dir()
            .and_then(|d| LinkConfig::in_dir(&d).ok())
            .unwrap_or_else(LinkConfig::ephemeral)
    }
}

impl Mode {
    /// `open`, `s1`, `s2` or `pair`.
    pub fn name(&self) -> &'static str {
        match self {
            Mode::Open => "open",
            Mode::Secure(Creds::Passphrase(_)) => "s1",
            Mode::Secure(Creds::Device(..)) => "s2",
            Mode::Secure(Creds::Pair(..)) => "pair",
        }
    }

    /// The credentials of an authenticated link.
    pub fn creds(&self) -> Option<&Creds> {
        match self {
            Mode::Open => None,
            Mode::Secure(c) => Some(c),
        }
    }
}
