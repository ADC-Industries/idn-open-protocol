//! TLS for the link (§I.4), the same over QUIC and inside a WebSocket: TLS 1.3, Ed25519
//! site certificates (§E.10.5). The Site Manager offers two ALPNs: `idn-industrial/1`
//! for enrolled machines (their certificate checked against the site CA and its CRL) and
//! `idn-industrial-join/1` for a machine joining with a token, which has no certificate
//! yet and pins the CA by the token's fingerprint.

use idn_common::sync::LockOk as _;
use idn_open_protocol::trust::pki::{Anchor, CertInfo, DeviceCreds, KeyFile, fingerprint};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use std::io;
use std::sync::{Arc, Mutex};

pub const ALPN_LINK: &[u8] = b"idn-industrial/1";
pub const ALPN_JOIN: &[u8] = b"idn-industrial-join/1";
/// The name TLS is told; certificates are checked by key, not by name.
pub const SERVER_NAME: &str = "idn-site-manager";

use idn_common::err::other as err;

fn provider() -> Arc<CryptoProvider> {
    idn_open_protocol::secure::crypto_provider()
}

/// The Site Manager's TLS: its authority certificate with the CA after it (a joining
/// machine checks the CA against its token), a client certificate asked for but not
/// required (a joining machine has none; [`peer`] says what was presented).
pub fn server_config(creds: &Arc<DeviceCreds>, ca_der: &[u8]) -> io::Result<rustls::ServerConfig> {
    let p = provider();
    let chain = vec![
        CertificateDer::from(creds.cert_der.clone()),
        CertificateDer::from(ca_der.to_vec()),
    ];
    let mut c = rustls::ServerConfig::builder_with_provider(p.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(err)?
        .with_client_cert_verifier(Arc::new(OptionalDevice {
            anchor: creds.anchor.clone(),
            algs: p.signature_verification_algorithms,
        }))
        .with_cert_resolver(creds.key.certified(chain, &p)?);
    c.alpn_protocols = vec![ALPN_LINK.to_vec(), ALPN_JOIN.to_vec()];
    // QUIC (quinn) wants either no early data or all of it allowed; none here.
    c.max_early_data_size = 0;
    Ok(c)
}

/// An enrolled machine's TLS: its certificate, and the server must be the site's
/// authority.
pub fn client_config(creds: &Arc<DeviceCreds>) -> io::Result<rustls::ClientConfig> {
    creds.client_config_for(ALPN_LINK, Some("authority"))
}

/// A joining machine's TLS: no certificate; the server must be the authority of the CA
/// whose fingerprint the token names. The CA certificate it presented is kept in `ca`.
pub fn join_config(
    ca_fingerprint: [u8; 32],
    ca: Arc<Mutex<Option<Vec<u8>>>>,
) -> io::Result<rustls::ClientConfig> {
    let p = provider();
    let mut c = rustls::ClientConfig::builder_with_provider(p.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(err)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCa {
            fingerprint: ca_fingerprint,
            seen: ca,
            algs: p.signature_verification_algorithms,
        }))
        .with_no_client_auth();
    c.alpn_protocols = vec![ALPN_JOIN.to_vec()];
    Ok(c)
}

/// What the peer of a Site Manager connection is: an enrolled machine (its certificate
/// checked, not revoked) or a joining one (`None`).
pub fn peer(anchor: &Anchor, certs: Option<&[CertificateDer<'_>]>) -> io::Result<Option<CertInfo>> {
    match certs.and_then(|c| c.first()) {
        None => Ok(None),
        Some(c) => anchor.verify_with(c.as_ref(), true).map(Some),
    }
}

/// A machine's key, stored with its certificate and the site's CA (§I.5).
pub fn creds(
    key: KeyFile,
    cert_der: Vec<u8>,
    ca_der: Vec<u8>,
    crl: Option<&[u8]>,
) -> io::Result<Arc<DeviceCreds>> {
    let anchor = Anchor::new(ca_der)?;
    if let Some(crl) = crl.filter(|c| !c.is_empty()) {
        anchor.accept_crl(crl)?;
    }
    Ok(Arc::new(DeviceCreds {
        key,
        cert_der,
        anchor: Arc::new(anchor),
    }))
}

#[derive(Debug)]
struct OptionalDevice {
    anchor: Arc<Anchor>,
    algs: WebPkiSupportedAlgorithms,
}

impl ClientCertVerifier for OptionalDevice {
    fn offer_client_auth(&self) -> bool {
        true
    }
    fn client_auth_mandatory(&self) -> bool {
        false
    }
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.anchor
            .verify_with(end_entity.as_ref(), true)
            .map(|_| ClientCertVerified::assertion())
            .map_err(|e| rustls::Error::General(e.to_string()))
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 is not used".into()))
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(m, c, d, &self.algs)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

#[derive(Debug)]
struct PinnedCa {
    fingerprint: [u8; 32],
    seen: Arc<Mutex<Option<Vec<u8>>>>,
    algs: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedCa {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fail = |m: &str| rustls::Error::General(m.into());
        let ca = intermediates
            .first()
            .ok_or_else(|| fail("the Site Manager sent no CA certificate"))?;
        let anchor = Anchor::new(ca.as_ref().to_vec()).map_err(|e| fail(&e.to_string()))?;
        let spki = anchor.ca_der_spki().map_err(|e| fail(&e.to_string()))?;
        let fp = fingerprint(&spki);
        if fp != idn_common::hex::hex(&self.fingerprint) {
            return Err(fail("not the site the token is for (another CA)"));
        }
        let info = anchor
            .verify_with(end_entity.as_ref(), false)
            .map_err(|e| fail(&e.to_string()))?;
        if info.kind != "authority" {
            return Err(fail("the server isn't the site's authority"));
        }
        *self.seen.lock_ok() = Some(ca.as_ref().to_vec());
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 is not used".into()))
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(m, c, d, &self.algs)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}
