//! A link side's offers and TLS checks: what it can use and offers, the peer's certificate
//! verified, and the certificate shown for each mode (§E.11.4).

use super::*;

impl Local {
    pub(super) fn new(link: &LinkConfig, secure: Option<&SecureConfig>) -> Self {
        Local::with_protocol(link, secure, ALPN, false)
    }

    /// As [`Local::new`] for another protocol on the same trust (the site link, R.18);
    /// `site_only`: nothing but site trust offered.
    pub(super) fn with_protocol(
        link: &LinkConfig,
        secure: Option<&SecureConfig>,
        alpn: &str,
        site_only: bool,
    ) -> Self {
        let site = secure
            .and_then(|s| s.site.clone())
            .and_then(|e| e.creds().map(|c| (c, e)));
        // An enrolled device offers only site trust (§E.10.7).
        let pass = match site {
            Some(_) => None,
            None => secure.and_then(|s| s.passphrase_key()),
        };
        // An enrolled device's key is its identity in mode open too (§E.11.4).
        let identity = secure
            .and_then(|s| s.site.as_ref())
            .and_then(|e| e.open_identity())
            .unwrap_or_else(|| link.identity.clone());
        let mut offers = Vec::new();
        if let Some((c, _)) = &site {
            offers.push((
                Kind::Site,
                format!("{alpn};s2={}", c.anchor.ca_id()).into_bytes(),
            ));
        }
        // Linked computers (§E.28), not in a site: one offer each, named by both short
        // fingerprints in order, so that the two machines' offers are the same bytes.
        let pairs: Vec<String> = match (&site, secure.and_then(|s| s.site.as_ref())) {
            (None, Some(e)) if !site_only => e.pairs().iter().map(|l| l.fp.clone()).collect(),
            _ => Vec::new(),
        };
        let me = identity.fingerprint();
        for fp in &pairs {
            let (a, b) = (
                crate::trust::pair::short(&me),
                crate::trust::pair::short(fp),
            );
            let (a, b) = if a <= b { (a, b) } else { (b, a) };
            offers.push((Kind::Pair, format!("{alpn};pair={a}+{b}").into_bytes()));
        }
        if let Some(k) = pass.as_ref().filter(|_| !site_only) {
            offers.push((
                Kind::Passphrase,
                format!("{alpn};s1={}", k.key_id).into_bytes(),
            ));
        }
        // A device in a site takes no unauthenticated link:
        // a revoked or foreign IDN device can't reach it as an "unknown" one.
        if !secure.is_some_and(|s| s.requires()) && site.is_none() && !site_only {
            offers.push((Kind::Open, alpn.as_bytes().to_vec()));
        }
        Local {
            site,
            pass,
            identity,
            pairs: Arc::new(pairs),
            enrollment: secure.and_then(|s| s.site.clone()),
            offers,
        }
    }

    /// Whether `cert` is a linked computer's, and, with an offer's ALPN, the one it
    /// names (§E.28).
    pub(super) fn linked(&self, cert: &[u8], alpn: Option<&[u8]>) -> bool {
        let fp = fingerprint_of(cert);
        if !self.pairs.contains(&fp) {
            return false;
        }
        alpn.is_none_or(|a| {
            String::from_utf8_lossy(a)
                .rsplit_once(";pair=")
                .is_some_and(|(_, both)| {
                    both.split('+').any(|f| f == crate::trust::pair::short(&fp))
                })
        })
    }

    /// Which mode a peer's certificate belongs to, under this side's credentials.
    pub(super) fn classify(&self, cert: &[u8]) -> Option<Kind> {
        if let Some((c, _)) = &self.site
            && c.anchor.verify(cert).is_ok()
        {
            return Some(Kind::Site);
        }
        let key = crate::secure::ed25519_public_key(cert)?;
        if self.pass.as_ref().is_some_and(|k| k.public_key() == &key) {
            return Some(Kind::Passphrase);
        }
        Some(Kind::Open)
    }

    fn allows(&self, k: Kind) -> bool {
        self.offers.iter().any(|(o, _)| *o == k)
    }

    fn certified(&self, k: Kind, provider: &CryptoProvider) -> Option<Arc<CertifiedKey>> {
        let (cert, key) = match k {
            Kind::Site => {
                let (c, _) = self.site.as_ref()?;
                (
                    CertificateDer::from(c.cert_der.clone()),
                    c.key.signing_key(provider).ok()?,
                )
            }
            Kind::Passphrase => {
                let p = self.pass.as_ref()?;
                (p.cert().clone(), p.signing_key(provider).ok()?)
            }
            Kind::Open | Kind::Pair => (
                self.identity.cert.clone(),
                self.identity.key.signing_key(provider).ok()?,
            ),
        };
        Some(Arc::new(CertifiedKey::new(vec![cert], key)))
    }

    pub(super) fn mode(&self, k: Kind, cert: &[u8]) -> Mode {
        match (k, &self.site, &self.pass) {
            (Kind::Site, Some((c, e)), _) => Mode::Secure(Creds::Device(c.clone(), e.clone())),
            (Kind::Passphrase, _, Some(p)) => Mode::Secure(Creds::Passphrase(p.clone())),
            (Kind::Pair, ..) => Mode::Secure(Creds::Pair(
                self.identity.clone(),
                self.pairs.clone(),
                self.enrollment
                    .as_ref()
                    .map(|e| e.pair_lease(&fingerprint_of(cert))),
            )),
            _ => Mode::Open,
        }
    }
}

impl Verifier {
    fn check(&self, cert: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        match self.local.classify(cert.as_ref()) {
            Some(k) if self.local.allows(k) => Ok(()),
            _ if self.local.allows(Kind::Pair) && self.local.linked(cert.as_ref(), None) => Ok(()),
            _ => Err(rustls::Error::General("certificate not acceptable".into())),
        }
    }
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|_| ServerCertVerified::assertion())
    }
    crate::secure::ed25519_tls13_signatures!(algs);
}

impl ClientCertVerifier for ClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        let chosen = *self.chosen.lock_ok();
        match KINDS.iter().position(|k| Some(*k) == chosen) {
            Some(i) => &self.hints[i],
            None => &[],
        }
    }
    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.inner
            .check(end_entity)
            .map(|_| ClientCertVerified::assertion())
    }
    crate::secure::ed25519_tls13_signatures!(inner.algs);
}

impl rustls::client::ResolvesClientCert for ClientCert {
    fn resolve(&self, hints: &[&[u8]], _: &[SignatureScheme]) -> Option<Arc<CertifiedKey>> {
        let k = KINDS
            .into_iter()
            .find(|k| hints.contains(&mode_hint(*k).as_ref()))?;
        if !self.local.allows(k) {
            return None;
        }
        self.local.certified(k, &self.provider)
    }
    fn has_certs(&self) -> bool {
        true
    }
}

impl rustls::server::ResolvesServerCert for ServerCert {
    fn resolve(&self, hello: rustls::server::ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let theirs: Vec<&[u8]> = hello.alpn().map(|a| a.collect()).unwrap_or_default();
        match self
            .local
            .offers
            .iter()
            .find(|(_, a)| theirs.contains(&a.as_slice()))
        {
            Some((k, _)) => {
                *self.chosen.lock_ok() = Some(*k);
                self.local.certified(*k, &self.provider)
            }
            // No mode in common: answer anyway, so that rustls's ALPN check ends the
            // handshake with no_application_protocol (a reason to fall back), not with
            // a certificate error (which would look like an attack).
            None => self.local.certified(Kind::Open, &self.provider),
        }
    }
}

/// [`server_config`] with the site link's transport.
pub(super) fn site_server_config(local: &Arc<Local>) -> io::Result<quinn::ServerConfig> {
    let mut c = server_config(local)?;
    c.transport_config(transport(true));
    Ok(c)
}

/// TLS alerts that mean authentication failed: bad, unsupported, revoked, expired or
/// unknown certificate, unknown CA, access denied.
pub(super) fn auth_alert(a: u8) -> bool {
    matches!(a, 42..=46 | 48 | 49)
}
