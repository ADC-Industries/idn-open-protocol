//! Trusting the server (VOICE-CLIENT.md §3.2): the SHA-256 of its leaf certificate,
//! given at enrolment, compared in full on every connection, whatever the certificate's
//! name or issuer (Voice's are self-signed, on an address). Without a pin, or for a
//! certificate that isn't the pinned one on the WebSocket (a tunnel's, from a public
//! authority), the ordinary checks against public authorities. QUIC (the link) has no
//! authority: its pin only.
//!
//! Voice on the link asks no client certificate (a handset signs in inside the
//! connection with its credential); should a listener ask anyway (the link's other modes
//! tell peers apart by them), the client shows a throwaway Ed25519 one.

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::sign::CertifiedKey;
use rustls::{DigitallySignedStruct, SignatureScheme};
use std::io;
use std::sync::{Arc, OnceLock};

/// The ALPN of Voice on the link.
pub(crate) const ALPN_VOICE: &[u8] = b"idn-voice/1";

/// rustls with ring for every configuration (as `idn_open_protocol::secure::
/// crypto_provider`, not a dependency here): ChaCha20 first on processors without AES
/// instructions (32-bit ARM handsets), as it is much quicker there.
pub(crate) fn provider() -> Arc<CryptoProvider> {
    static P: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
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

pub(crate) fn sha256(b: &[u8]) -> [u8; 32] {
    let d = ring::digest::digest(&ring::digest::SHA256, b);
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    out
}

/// The pin first; then, where allowed, public authorities.
#[derive(Debug)]
struct Pinned {
    pin: Option<[u8; 32]>,
    authorities: Option<Arc<WebPkiServerVerifier>>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if self.pin.is_some_and(|p| p == sha256(end_entity.as_ref())) {
            return Ok(ServerCertVerified::assertion());
        }
        match &self.authorities {
            Some(a) => a.verify_server_cert(end_entity, intermediates, name, ocsp, now),
            None => Err(rustls::Error::General(
                "the site's certificate changed: enrol again".into(),
            )),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn authorities(provider: &Arc<CryptoProvider>) -> io::Result<Arc<WebPkiServerVerifier>> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .map_err(io::Error::other)
}

/// The WebSocket's TLS: `pin` (the enrolment's `cert_sha256`) or public authorities.
pub(crate) fn websocket_config(pin: Option<[u8; 32]>) -> io::Result<Arc<rustls::ClientConfig>> {
    let provider = provider();
    let verifier = Pinned {
        pin,
        authorities: Some(authorities(&provider)?),
        provider: provider.clone(),
    };
    let mut c = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    c.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(c))
}

/// A throwaway Ed25519 certificate, shown only if the listener asks for one.
#[derive(Debug)]
struct Throwaway(Arc<CertifiedKey>);

impl rustls::client::ResolvesClientCert for Throwaway {
    fn resolve(&self, _: &[&[u8]], schemes: &[SignatureScheme]) -> Option<Arc<CertifiedKey>> {
        schemes
            .contains(&SignatureScheme::ED25519)
            .then(|| self.0.clone())
    }

    fn has_certs(&self) -> bool {
        true
    }
}

fn throwaway(provider: &CryptoProvider) -> io::Result<Arc<CertifiedKey>> {
    let bad = |e: rcgen::Error| io::Error::other(format!("a client certificate: {e}"));
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).map_err(bad)?;
    let mut params =
        rcgen::CertificateParams::new(vec!["idn-voice-client".to_owned()]).map_err(bad)?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "IDN Voice handset");
    let cert = params.self_signed(&key).map_err(bad)?;
    let der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
    let signer = provider
        .key_provider
        .load_private_key(der)
        .map_err(io::Error::other)?;
    Ok(Arc::new(CertifiedKey::new(
        vec![cert.der().clone()],
        signer,
    )))
}

/// The link's TLS (QUIC, TLS 1.3): the listener's certificate pinned (it is Ed25519),
/// `idn-voice/1`, a throwaway certificate if asked. No 0-RTT (a replayed sign-in or
/// command).
pub(crate) fn link_config(pin: [u8; 32]) -> io::Result<rustls::ClientConfig> {
    // One certificate for the process's life: made once, it costs nothing per connection.
    static CERT: OnceLock<Arc<CertifiedKey>> = OnceLock::new();
    let provider = provider();
    let cert = match CERT.get() {
        Some(c) => c.clone(),
        None => {
            let c = throwaway(&provider)?;
            CERT.get_or_init(|| c).clone()
        }
    };
    let verifier = Pinned {
        pin: Some(pin),
        authorities: None,
        provider: provider.clone(),
    };
    let mut c = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(io::Error::other)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_client_cert_resolver(Arc::new(Throwaway(cert)));
    c.alpn_protocols = vec![ALPN_VOICE.to_vec()];
    c.enable_early_data = false;
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pin_decides_and_the_throwaway_certificate_is_ed25519() {
        let provider = provider();
        let cert = throwaway(&provider).unwrap();
        let der = cert.cert[0].clone();
        let name = ServerName::try_from("192.168.1.5").unwrap();
        let check = |pin: Option<[u8; 32]>, authorities| {
            Pinned {
                pin,
                authorities,
                provider: provider.clone(),
            }
            .verify_server_cert(&der, &[], &name, &[], UnixTime::now())
            .is_ok()
        };
        assert!(check(Some(sha256(der.as_ref())), None));
        assert!(!check(Some([0; 32]), None));
        // Not pinned and self-signed: no public authority vouches for it.
        assert!(!check(None, Some(authorities(&provider).unwrap())));
        assert_eq!(cert.key.algorithm(), rustls::SignatureAlgorithm::ED25519);
        assert!(link_config([1; 32]).is_ok());
        assert!(websocket_config(None).is_ok());
    }
}
