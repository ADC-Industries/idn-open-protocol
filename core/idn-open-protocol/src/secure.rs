//! Optional encryption between IDN devices that share a site passphrase.
//!
//! Give every IDN device on the site the same passphrase, as one [`SecureConfig`] on
//! the [`PublisherConfig`](crate::api::PublisherConfig) and on each
//! [`InputConfig`](crate::input::InputConfig). Then:
//!
//! - IDN devices with the same passphrase authenticate each other and encrypt
//!   everything: control, tally, video and audio. Video and audio travel over QUIC with
//!   TLS 1.3. If UDP is blocked, they fall back to the encrypted control connection
//!   ([`InputStats::secure_fallback`](crate::input::InputStats::secure_fallback)).
//!   They are never sent unencrypted.
//! - Devices without it (and every NDI device) still connect, unencrypted. That is the
//!   default, "encrypt when possible".
//! - [`SecureConfig::require`] turns that into "require encryption": everything that
//!   can't encrypt is refused.
//! - Once two devices have encrypted, they refuse to talk unencrypted later
//!   ([`StickyStore`]), so an attacker can't quietly strip the encryption.
//!
//! Without a passphrase none of this is used and nothing changes on the network.
//!
//! ```no_run
//! use idn_open_protocol::api::{Publisher, PublisherConfig};
//! use idn_open_protocol::input::{Input, InputConfig};
//! use idn_open_protocol::secure::{SecureConfig, StickyStore};
//!
//! # fn main() -> std::io::Result<()> {
//! // Slow on purpose (about 0.1 s on a modern PC): do it once at start-up.
//! let mut sc = SecureConfig::new("St Mark's AV team")?;
//! sc.sticky = StickyStore::with_file("trusted-devices.txt".into());
//! let publisher = Publisher::new(PublisherConfig { secure: Some(sc.clone()), ..Default::default() })?;
//! let input = Input::open(InputConfig { secure: Some(sc.clone()), ..InputConfig::new("CAM-1 (Main)") })?;
//! // Later, in the UI:
//! println!("encrypted: {}", input.stats().encrypted);
//! # Ok(())
//! # }
//! ```

use idn_common::sync::LockOk as _;
use rustls::DistinguishedName;
use rustls::client::danger::{ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use std::collections::BTreeSet;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

mod config;
mod negotiate;
mod tls;
pub use config::{Creds, SecureConfig, StickyStore};
pub(crate) use negotiate::tell_refused;
pub use negotiate::{
    Negotiated, REFUSAL_HOLD, negotiate_receiver, negotiate_sender, remember_peer,
};
use tls::SiteVerifier;
pub(crate) use tls::loopback_pair;
pub use tls::{accept_tls, connect_tls, wrap, wrap_with_peer};

/// ALPN of the TLS session on the control connection (§E.8.3).
#[doc(hidden)]
pub const CONTROL_ALPN: &[u8] = b"idn-control/1";

/// The keys every holder of the site passphrase derives from it. Create it through
/// [`SecureConfig::new`]. It never prints key material.
pub struct SiteKey {
    /// A short public identifier of the passphrase (16 hex digits). Devices with the same
    /// passphrase have the same id. It is safe to show, e.g. "site key 6ccc95d4…", and
    /// helps spot a device configured with a different passphrase.
    pub key_id: String,
    cert: CertificateDer<'static>,
    /// The derived key: here, or held by the IDN Node.
    key: crate::trust::pki::KeyFile,
    public_key: [u8; 32],
    /// `Kjoin` (§E.10.4): proves knowledge of the passphrase when joining site trust;
    /// `None` when the IDN Node holds the passphrase (it joins, programs don't).
    join_key: Option<[u8; 32]>,
}

impl std::fmt::Debug for SiteKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SiteKey({})", self.key_id) // never print key material
    }
}

fn hkdf32(psk: &[u8; 32], info: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    hkdf::Hkdf::<sha2::Sha256>::new(None, psk)
        .expand(info.as_bytes(), &mut out)
        .expect("32 bytes is a valid HKDF length");
    out
}

/// The Ed25519 public key of a certificate, from its parsed SubjectPublicKeyInfo: the
/// key the peer's handshake signature is checked against. (Searching the DER for the
/// SPKI byte pattern instead let a certificate carry the site key in another field, such
/// as the issuer name, while signing with its own key.)
pub(crate) fn ed25519_public_key(cert_der: &[u8]) -> Option<[u8; 32]> {
    let (rest, c) = x509_parser::parse_x509_certificate(cert_der).ok()?;
    if !rest.is_empty() {
        return None;
    }
    crate::trust::pki::public_from_spki(c.public_key().raw)
}

impl SiteKey {
    /// Derives the site key from the passphrase. Deliberately slow and memory-hard
    /// (Argon2id, 64 MiB), so a stolen key id can't be used to guess the passphrase
    /// quickly: do it once at start-up, not per connection. Any passphrase is accepted;
    /// longer is stronger.
    pub fn from_passphrase(passphrase: &str) -> io::Result<Arc<Self>> {
        Self::from_psk(&Self::psk(passphrase)?)
    }

    /// The Argon2id stretch of the passphrase that every site key is derived from. The
    /// IDN Node keeps this instead of the passphrase itself.
    #[doc(hidden)]
    pub fn psk(passphrase: &str) -> io::Result<[u8; 32]> {
        let params = argon2::Params::new(64 * 1024, 3, 1, Some(32))
            .map_err(|e| io::Error::other(e.to_string()))?;
        let a2 = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
        let mut psk = [0u8; 32];
        a2.hash_password_into(passphrase.as_bytes(), b"IDN secure/1 site key", &mut psk)
            .map_err(|e| io::Error::other(e.to_string()))?;
        Ok(psk)
    }

    /// The site key from the stretched passphrase ([`SiteKey::psk`]).
    #[doc(hidden)]
    pub fn from_psk(psk: &[u8; 32]) -> io::Result<Arc<Self>> {
        let psk = *psk;
        let id = hkdf32(&psk, "IDN secure/1 id");
        let join_key = hkdf32(&psk, "IDN trust join");
        let key_id = idn_common::hex::hex(&id[..8]);
        let seed = hkdf32(&psk, "IDN secure/1 cert key");
        // PKCS#8 v1 wrapping of an Ed25519 seed (RFC 8410).
        let mut key_pkcs8 = vec![
            0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22,
            0x04, 0x20,
        ];
        key_pkcs8.extend_from_slice(&seed);
        let kp = rcgen::KeyPair::try_from(key_pkcs8.as_slice())
            .map_err(|e| io::Error::other(e.to_string()))?;
        // A deterministic certificate (§E.8.1 SHOULD): fixed serial, validity and names.
        let mut params = rcgen::CertificateParams::new(vec!["idn-site".to_owned()])
            .map_err(|e| io::Error::other(e.to_string()))?;
        params.serial_number = Some(rcgen::SerialNumber::from(1u64));
        params.not_before = rcgen::date_time_ymd(2000, 1, 1);
        params.not_after = rcgen::date_time_ymd(9999, 12, 31);
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "IDN site");
        let cert = params
            .self_signed(&kp)
            .map_err(|e| io::Error::other(e.to_string()))?;
        let cert = cert.der().clone();
        let public_key = ed25519_public_key(&cert)
            .ok_or_else(|| io::Error::other("derived certificate has no Ed25519 key"))?;
        Ok(Arc::new(SiteKey {
            key_id,
            cert,
            key: crate::trust::pki::KeyFile::from_pkcs8(key_pkcs8)?,
            public_key,
            join_key: Some(join_key),
        }))
    }

    /// The site key held by the IDN Node: its id and certificate (public), signing done by
    /// the Node (spec/extensions/idn-node.md §N.4.4).
    pub(crate) fn remote(
        key_id: &str,
        cert_der: Vec<u8>,
        signer: Arc<dyn crate::trust::pki::RemoteSigner>,
    ) -> io::Result<Arc<Self>> {
        let cert = CertificateDer::from(cert_der);
        let public_key = ed25519_public_key(&cert)
            .ok_or_else(|| io::Error::other("site certificate has no Ed25519 key"))?;
        Ok(Arc::new(SiteKey {
            key_id: key_id.to_owned(),
            cert,
            key: crate::trust::pki::KeyFile::remote(public_key, signer, "site"),
            public_key,
            join_key: None,
        }))
    }

    /// Signs a TLS handshake message with the derived key (the IDN Node, for programs).
    #[doc(hidden)]
    pub fn sign(&self, message: &[u8]) -> io::Result<Vec<u8>> {
        self.key.sign(message)
    }

    /// HMAC-SHA256 with `Kjoin` over `role || device SPKI || CA SPKI` (§E.10.4); zeros for a
    /// key held by the IDN Node, which never joins from a program.
    pub(crate) fn join_mac(&self, role: &str, device_spki: &[u8], ca_spki: &[u8]) -> [u8; 32] {
        let k = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &self.join_key.unwrap_or([0; 32]));
        let mut ctx = ring::hmac::Context::with_key(&k);
        for part in [role.as_bytes(), device_spki, ca_spki] {
            ctx.update(part);
        }
        ctx.sign().as_ref().try_into().expect("32-byte HMAC")
    }

    /// The device's passphrase proof, second form (§E.10.4): HMAC-SHA256 with `Kjoin` over
    /// `"device2" || device SPKI || CA SPKI || name || 0 || kind || 0 || at` (`at` the
    /// device's Unix time in seconds, as decimal). The name, kind and time are covered: the
    /// first form, sent in plain text, could be replayed under another name.
    pub(crate) fn join_mac2(
        &self,
        device_spki: &[u8],
        ca_spki: &[u8],
        name: &str,
        kind: &str,
        at: u64,
    ) -> [u8; 32] {
        let k = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &self.join_key.unwrap_or([0; 32]));
        let mut ctx = ring::hmac::Context::with_key(&k);
        let at = at.to_string();
        for part in [
            b"device2".as_slice(),
            device_spki,
            ca_spki,
            name.as_bytes(),
            &[0],
            kind.as_bytes(),
            &[0],
            at.as_bytes(),
        ] {
            ctx.update(part);
        }
        ctx.sign().as_ref().try_into().expect("32-byte HMAC")
    }

    /// The derived site certificate (§E.8.1).
    #[doc(hidden)]
    pub fn cert(&self) -> &CertificateDer<'static> {
        &self.cert
    }

    /// The derived certificate's Ed25519 public key.
    pub(crate) fn public_key(&self) -> &[u8; 32] {
        &self.public_key
    }

    /// The derived key, for a TLS configuration's resolver.
    pub(crate) fn certified(
        &self,
        provider: &CryptoProvider,
    ) -> io::Result<Arc<rustls::sign::SingleCertAndKey>> {
        self.key.certified(vec![self.cert.clone()], provider)
    }

    /// The derived key as rustls's signing key.
    pub(crate) fn signing_key(
        &self,
        provider: &CryptoProvider,
    ) -> io::Result<Arc<dyn rustls::sign::SigningKey>> {
        self.key.signing_key(provider)
    }

    /// TLS client configuration for the receiver side.
    #[doc(hidden)]
    pub fn client_config(self: &Arc<Self>) -> io::Result<rustls::ClientConfig> {
        let provider = crate::secure::crypto_provider();
        let mut c = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(io::Error::other)?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(SiteVerifier::new(self.clone(), &provider)))
            .with_client_cert_resolver(self.certified(&provider)?);
        c.alpn_protocols = vec![CONTROL_ALPN.to_vec()];
        Ok(c)
    }

    /// TLS server configuration for the sender side; client certificates are required.
    #[doc(hidden)]
    pub fn server_config(self: &Arc<Self>) -> io::Result<rustls::ServerConfig> {
        let provider = crate::secure::crypto_provider();
        let mut c = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(io::Error::other)?
            .with_client_cert_verifier(Arc::new(SiteVerifier::new(self.clone(), &provider)))
            .with_cert_resolver(self.certified(&provider)?);
        c.alpn_protocols = vec![CONTROL_ALPN.to_vec()];
        Ok(c)
    }
}

/// The TLS crypto provider every IDN connection and client uses: ring, with
/// ChaCha20-Poly1305 first on CPUs without AES instructions (§E.11.4; x86-64 and 64-bit
/// ARM have them). Made once.
pub fn crypto_provider() -> Arc<CryptoProvider> {
    static P: std::sync::OnceLock<Arc<CryptoProvider>> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        let mut p = rustls::crypto::ring::default_provider();
        if !cfg!(any(target_arch = "x86_64", target_arch = "aarch64")) {
            p.cipher_suites
                .sort_by_key(|s| s.suite() != rustls::CipherSuite::TLS13_CHACHA20_POLY1305_SHA256);
        }
        Arc::new(p)
    })
    .clone()
}

/// The signature checks of a certificate verifier that takes Ed25519 over TLS 1.3 only
/// (every IDN connection's): TLS 1.2 refused, TLS 1.3 checked with the algorithms at
/// `self.FIELD` (`ed25519_tls13_signatures!(algs)` inside the `impl`). One copy of the
/// three methods each verifier had written out.
macro_rules! ed25519_tls13_signatures {
    ($($algs:ident).+) => {
        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &::rustls::pki_types::CertificateDer<'_>,
            _: &::rustls::DigitallySignedStruct,
        ) -> Result<::rustls::client::danger::HandshakeSignatureValid, ::rustls::Error> {
            Err(::rustls::Error::General("TLS 1.2 is not used".into()))
        }
        fn verify_tls13_signature(
            &self,
            m: &[u8],
            c: &::rustls::pki_types::CertificateDer<'_>,
            d: &::rustls::DigitallySignedStruct,
        ) -> Result<::rustls::client::danger::HandshakeSignatureValid, ::rustls::Error> {
            ::rustls::crypto::verify_tls13_signature(m, c, d, &self.$($algs).+)
        }
        fn supported_verify_schemes(&self) -> Vec<::rustls::SignatureScheme> {
            vec![::rustls::SignatureScheme::ED25519]
        }
    };
}
pub(crate) use ed25519_tls13_signatures;

#[cfg(test)]
mod tests;
