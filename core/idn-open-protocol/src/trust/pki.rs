//! Keys, certificates and revocation lists for site trust (§E.10.1, §E.10.5, §E.10.6).
//!
//! Everything is Ed25519. Certificates are issued with rcgen and checked with
//! x509-parser; TLS handshake signatures are checked by rustls, as for `secure/1`.

use idn_common::err::other as err;
use idn_common::sync::RwLockOk as _;
use rustls::client::danger::{ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DistinguishedName, SignatureScheme};
use std::collections::BTreeSet;
use std::io;
use std::path::Path;
use std::sync::{Arc, RwLock};

/// What a handshake's error starts with when the IDN Node refused to sign for it.
pub(crate) const NODE_SIGNING: &str = "signing by the IDN Node";

/// DER prefix of an Ed25519 SubjectPublicKeyInfo; the 32-byte key follows.
const SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// The DER SubjectPublicKeyInfo of an Ed25519 public key.
pub fn spki(public: &[u8; 32]) -> Vec<u8> {
    let mut v = SPKI_PREFIX.to_vec();
    v.extend_from_slice(public);
    v
}

/// The Ed25519 public key in a DER SubjectPublicKeyInfo.
pub fn public_from_spki(spki: &[u8]) -> Option<[u8; 32]> {
    spki.strip_prefix(&SPKI_PREFIX[..])?.try_into().ok()
}

/// Fingerprint (§E.10.1): SHA-256 of the DER SubjectPublicKeyInfo, 64 lower-case hex digits.
pub fn fingerprint(spki: &[u8]) -> String {
    crate::hash::sha256_hex(spki)
}

/// Signs with a key held by another process: the IDN Node (spec/extensions/idn-node.md
/// §N.4.4). `key` names which of its keys: `device`, `site` (the passphrase key) or
/// `link`.
pub trait RemoteSigner: Send + Sync + std::fmt::Debug {
    /// The Ed25519 signature of `message` (a TLS 1.3 `CertificateVerify` input).
    fn sign(&self, key: &str, message: &[u8]) -> io::Result<Vec<u8>>;
}

/// Where a key's secret half is.
#[derive(Clone)]
enum Secret {
    /// Here, as PKCS#8.
    Local(Vec<u8>),
    /// In another process, which signs on request.
    Remote(Arc<dyn RemoteSigner>, &'static str),
}

/// An Ed25519 key pair kept as PKCS#8, or the public half of one held by the IDN Node.
#[derive(Clone)]
pub struct KeyFile {
    secret: Secret,
    /// The public key.
    pub public: [u8; 32],
}

impl std::fmt::Debug for KeyFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "KeyFile({})", &fingerprint(&spki(&self.public))[..16]) // never the secret
    }
}

impl KeyFile {
    /// A new random key pair.
    pub fn generate() -> io::Result<Self> {
        let pkcs8 =
            ring::signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
                .map_err(err)?;
        Self::from_pkcs8(pkcs8.as_ref().to_vec())
    }

    pub(crate) fn from_pkcs8(pkcs8: Vec<u8>) -> io::Result<Self> {
        use ring::signature::KeyPair;
        let kp =
            ring::signature::Ed25519KeyPair::from_pkcs8_maybe_unchecked(&pkcs8).map_err(err)?;
        let public = kp.public_key().as_ref().try_into().map_err(err)?;
        Ok(KeyFile {
            secret: Secret::Local(pkcs8),
            public,
        })
    }

    /// The public half of a key held by another process, which signs for it.
    pub(crate) fn remote(
        public: [u8; 32],
        signer: Arc<dyn RemoteSigner>,
        which: &'static str,
    ) -> Self {
        KeyFile {
            secret: Secret::Remote(signer, which),
            public,
        }
    }

    /// Whether the secret half is in this process.
    pub fn is_local(&self) -> bool {
        matches!(self.secret, Secret::Local(_))
    }

    /// The Ed25519 signature of `message`, made here or by the key's holder.
    pub fn sign(&self, message: &[u8]) -> io::Result<Vec<u8>> {
        match &self.secret {
            Secret::Local(pkcs8) => {
                let kp = ring::signature::Ed25519KeyPair::from_pkcs8_maybe_unchecked(pkcs8)
                    .map_err(err)?;
                Ok(kp.sign(message).as_ref().to_vec())
            }
            Secret::Remote(s, which) => s.sign(which, message),
        }
    }

    /// The key for rustls's handshakes.
    pub(crate) fn signing_key(
        &self,
        provider: &CryptoProvider,
    ) -> io::Result<Arc<dyn rustls::sign::SigningKey>> {
        match &self.secret {
            Secret::Local(pkcs8) => provider
                .key_provider
                .load_private_key(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                    pkcs8.clone(),
                )))
                .map_err(err),
            Secret::Remote(..) => Ok(Arc::new(RemoteKey(self.clone()))),
        }
    }

    /// A certificate chain with this key, for a TLS configuration's resolver.
    pub fn certified(
        &self,
        chain: Vec<CertificateDer<'static>>,
        provider: &CryptoProvider,
    ) -> io::Result<Arc<rustls::sign::SingleCertAndKey>> {
        let ck = rustls::sign::CertifiedKey::new(chain, self.signing_key(provider)?);
        Ok(Arc::new(rustls::sign::SingleCertAndKey::from(ck)))
    }

    /// Loads the key at `path`, or creates it there (readable by this user only).
    pub fn load_or_create(path: &Path) -> io::Result<Self> {
        match std::fs::read(path) {
            Ok(b) => Self::from_pkcs8(b),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let k = Self::generate()?;
                write_private(path, k.pkcs8())?;
                Ok(k)
            }
            Err(e) => Err(e),
        }
    }

    /// The SubjectPublicKeyInfo.
    pub fn spki(&self) -> Vec<u8> {
        spki(&self.public)
    }

    /// The fingerprint.
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.spki())
    }

    /// For issuing or self-signing certificates: only a key held here can.
    pub(crate) fn rcgen(&self) -> io::Result<rcgen::KeyPair> {
        match &self.secret {
            Secret::Local(pkcs8) => rcgen::KeyPair::try_from(pkcs8.as_slice()).map_err(err),
            Secret::Remote(..) => Err(err("this key is held by the IDN Node")),
        }
    }

    /// The PKCS#8 bytes (for a CA backup); empty for a key held by the IDN Node.
    pub fn pkcs8(&self) -> &[u8] {
        match &self.secret {
            Secret::Local(pkcs8) => pkcs8,
            Secret::Remote(..) => &[],
        }
    }
}

/// A key held by another process, as rustls's signing key: Ed25519 only.
#[derive(Debug)]
struct RemoteKey(KeyFile);

impl rustls::sign::SigningKey for RemoteKey {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn rustls::sign::Signer>> {
        offered
            .contains(&SignatureScheme::ED25519)
            .then(|| Box::new(RemoteSign(self.0.clone())) as Box<dyn rustls::sign::Signer>)
    }

    fn algorithm(&self) -> rustls::SignatureAlgorithm {
        rustls::SignatureAlgorithm::ED25519
    }
}

#[derive(Debug)]
struct RemoteSign(KeyFile);

impl rustls::sign::Signer for RemoteSign {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error> {
        self.0
            .sign(message)
            .map_err(|e| rustls::Error::General(format!("{NODE_SIGNING}: {e}")))
    }

    fn scheme(&self) -> SignatureScheme {
        SignatureScheme::ED25519
    }
}

/// Writes a secret file readable by this user only, creating parent directories
/// ([`idn_common::fsutil::write_private`]).
pub fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    idn_common::fsutil::write_private(path, data)
}

/// Whether `n` may be a DNS name in a certificate: letters, digits, `-` and `.` in
/// labels of 1–63, at most 253 in all, no wildcards.
pub(crate) fn dns_name_ok(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 253
        && n.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// `der` as PEM with this label (`CERTIFICATE`): base64 in lines of 64.
pub fn pem(label: &str, der: &[u8]) -> String {
    let b = idn_common::text::base64(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for line in b.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).unwrap_or(""));
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

/// An Ed25519 public key for rcgen to put in a certificate.
struct RawKey([u8; 32]);

impl rcgen::PublicKeyData for RawKey {
    fn der_bytes(&self) -> &[u8] {
        &self.0
    }
    fn algorithm(&self) -> &'static rcgen::SignatureAlgorithm {
        &rcgen::PKCS_ED25519
    }
}

/// What a device certificate says (§E.10.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertInfo {
    /// Serial number bytes, as encoded.
    pub serial: Vec<u8>,
    /// The device fingerprint (from `urn:idn:fp:`, checked against the key).
    pub fingerprint: String,
    /// CN.
    pub name: String,
    /// `output`, `input`, `app` or `authority`.
    pub kind: String,
    /// Carries `urn:idn:role:admin`.
    pub admin: bool,
}

/// The site certificate authority, inside the Discovery Server (§E.10.1).
pub struct Ca {
    key: KeyFile,
    kp: rcgen::KeyPair,
    cert: rcgen::Certificate,
}

fn ca_params() -> io::Result<rcgen::CertificateParams> {
    let mut p = rcgen::CertificateParams::new(Vec::<String>::new()).map_err(err)?;
    p.distinguished_name = rcgen::DistinguishedName::new();
    p.distinguished_name
        .push(rcgen::DnType::CommonName, "IDN site authority");
    p.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Constrained(0));
    p.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
        rcgen::KeyUsagePurpose::DigitalSignature,
    ];
    // Fixed serial and validity: the same key always gives the same certificate.
    p.serial_number = Some(rcgen::SerialNumber::from(1u64));
    p.not_before = rcgen::date_time_ymd(2000, 1, 1);
    p.not_after = rcgen::date_time_ymd(9999, 12, 31);
    Ok(p)
}

impl Ca {
    /// The CA with this key (its certificate is rebuilt identically each time).
    pub fn from_key(key: KeyFile) -> io::Result<Self> {
        let kp = key.rcgen()?;
        let cert = ca_params()?.self_signed(&kp).map_err(err)?;
        Ok(Ca { key, kp, cert })
    }

    /// What signs as the CA: its name, key identifier and key usages, and its key.
    fn issuer(&self) -> io::Result<rcgen::Issuer<'static, &rcgen::KeyPair>> {
        Ok(rcgen::Issuer::new(ca_params()?, &self.kp))
    }

    /// The CA certificate (DER). (rcgen 0.14 writes its key usage as DER has it, `03 02
    /// 01 86`; 0.13 wrote `03 03 07 86 00`: the bytes differ from a site's earlier ones,
    /// its key, name and every fingerprint (all from the key) don't.)
    pub fn cert_der(&self) -> Vec<u8> {
        self.cert.der().to_vec()
    }

    /// The CA key, for backups.
    pub fn key(&self) -> &KeyFile {
        &self.key
    }

    /// The CA fingerprint (§E.10.1).
    pub fn fingerprint(&self) -> String {
        self.key.fingerprint()
    }

    /// Issues a device certificate (§E.10.5): random 128-bit serial, CN = name, SAN URIs
    /// for fingerprint, kind and (optionally) admin role, valid from now until 9999.
    pub fn issue(
        &self,
        device_spki: &[u8],
        name: &str,
        kind: &str,
        admin: bool,
    ) -> io::Result<(Vec<u8>, CertInfo)> {
        let public = public_from_spki(device_spki).ok_or_else(|| err("not an Ed25519 key"))?;
        let fp = fingerprint(device_spki);
        let mut serial = crate::random::secure_random::<16>();
        serial[0] &= 0x7f; // positive INTEGER
        serial[0] |= 0x01; // and no leading zero byte to strip
        let mut sans = vec![format!("urn:idn:fp:{fp}"), format!("urn:idn:kind:{kind}")];
        if admin {
            sans.push("urn:idn:role:admin".into());
        }
        let mut p = rcgen::CertificateParams::new(Vec::<String>::new()).map_err(err)?;
        p.subject_alt_names = sans
            .into_iter()
            .map(|u| {
                rcgen::string::Ia5String::try_from(u)
                    .map(rcgen::SanType::URI)
                    .map_err(err)
            })
            .collect::<io::Result<_>>()?;
        p.distinguished_name = rcgen::DistinguishedName::new();
        p.distinguished_name.push(rcgen::DnType::CommonName, name);
        p.serial_number = Some(rcgen::SerialNumber::from_slice(&serial));
        p.not_before = time::OffsetDateTime::now_utc();
        p.not_after = rcgen::date_time_ymd(9999, 12, 31) + time::Duration::seconds(86_399);
        p.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
        p.extended_key_usages = vec![
            rcgen::ExtendedKeyUsagePurpose::ClientAuth,
            rcgen::ExtendedKeyUsagePurpose::ServerAuth,
        ];
        p.use_authority_key_identifier_extension = true;
        let cert = p.signed_by(&RawKey(public), &self.issuer()?).map_err(err)?;
        let der = cert.der().to_vec();
        let info = CertInfo {
            serial: serial.to_vec(),
            fingerprint: fp,
            name: name.to_owned(),
            kind: kind.to_owned(),
            admin,
        };
        Ok((der, info))
    }

    /// Issues a machine's certificate for its HTTPS ports (§E.10.11): the device key
    /// `device_spki`, CN = name, DNS and IP SubjectAltNames for `names` and `ips` and
    /// `urn:idn:lan:<fingerprint>`, server authentication only, valid from an hour ago
    /// (clocks differ) for [`LAN_CERT_LIFE`]. It has no `urn:idn:fp:`, so `secure/2`
    /// never takes it as a device certificate ([`Anchor::verify`]).
    pub fn issue_lan(
        &self,
        device_spki: &[u8],
        name: &str,
        names: &[String],
        ips: &[std::net::IpAddr],
    ) -> io::Result<Vec<u8>> {
        let public = public_from_spki(device_spki).ok_or_else(|| err("not an Ed25519 key"))?;
        let fp = fingerprint(device_spki);
        let mut serial = crate::random::secure_random::<16>();
        serial[0] &= 0x7f;
        serial[0] |= 0x01;
        let mut p = rcgen::CertificateParams::new(Vec::<String>::new()).map_err(err)?;
        let mut sans = Vec::new();
        for n in names {
            sans.push(rcgen::SanType::DnsName(
                rcgen::string::Ia5String::try_from(n.as_str()).map_err(err)?,
            ));
        }
        sans.extend(ips.iter().map(|ip| rcgen::SanType::IpAddress(*ip)));
        sans.push(rcgen::SanType::URI(
            rcgen::string::Ia5String::try_from(format!("{LAN_URN}{fp}")).map_err(err)?,
        ));
        p.subject_alt_names = sans;
        p.distinguished_name = rcgen::DistinguishedName::new();
        p.distinguished_name.push(rcgen::DnType::CommonName, name);
        p.serial_number = Some(rcgen::SerialNumber::from_slice(&serial));
        let now = time::OffsetDateTime::now_utc();
        p.not_before = now - time::Duration::hours(1);
        p.not_after = now + time::Duration::seconds(LAN_CERT_LIFE.as_secs() as i64);
        p.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
        p.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        p.use_authority_key_identifier_extension = true;
        let cert = p.signed_by(&RawKey(public), &self.issuer()?).map_err(err)?;
        Ok(cert.der().to_vec())
    }

    /// A CRL (§E.10.6) with this number, listing these serials.
    pub fn crl(&self, number: u64, revoked: &[Vec<u8>]) -> io::Result<Vec<u8>> {
        let now = time::OffsetDateTime::now_utc();
        let p = rcgen::CertificateRevocationListParams {
            this_update: now,
            next_update: rcgen::date_time_ymd(9999, 12, 31),
            crl_number: rcgen::SerialNumber::from(number),
            issuing_distribution_point: None,
            revoked_certs: revoked
                .iter()
                .map(|s| rcgen::RevokedCertParams {
                    serial_number: rcgen::SerialNumber::from_slice(s),
                    revocation_time: now,
                    reason_code: None,
                    invalidity_date: None,
                })
                .collect(),
            key_identifier_method: rcgen::KeyIdMethod::Sha256,
        };
        Ok(p.signed_by(&self.issuer()?).map_err(err)?.der().to_vec())
    }
}

/// How long a machine's certificate for its HTTPS ports lives (§E.10.11): renewed when
/// half of it is left, so a revoked machine's lapses within this time.
pub const LAN_CERT_LIFE: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

/// The SubjectAltName URI naming the machine (its fingerprint follows) in a certificate
/// for its HTTPS ports (§E.10.11).
pub const LAN_URN: &str = "urn:idn:lan:";

/// What a machine's certificate for its HTTPS ports says (§E.10.11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanCert {
    /// The certificate (DER).
    pub der: Vec<u8>,
    /// Its DNS names.
    pub names: Vec<String>,
    /// Its IP addresses.
    pub ips: Vec<std::net::IpAddr>,
    /// Unix seconds it is valid until.
    pub not_after: i64,
}

impl LanCert {
    /// Whether it is valid now, and for at least `left` more.
    pub fn valid_for(&self, left: std::time::Duration) -> bool {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        self.not_after > now.saturating_add(left.as_secs() as i64)
    }
}

/// A verified CRL: its number and revoked serials.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Crl {
    /// `cRLNumber`; 0 = none received yet.
    pub number: u64,
    /// Revoked serial numbers.
    pub serials: BTreeSet<Vec<u8>>,
    /// The CRL as received (for caching), empty if none.
    pub der: Vec<u8>,
}

/// The pinned site CA and the latest CRL: what a device trusts (§E.10.4, §E.10.6).
#[derive(Debug)]
pub struct Anchor {
    /// The CA certificate (DER).
    pub ca_der: Vec<u8>,
    /// Its fingerprint.
    pub ca_fp: String,
    /// The newest verified CRL.
    pub crl: RwLock<Crl>,
}

fn strip_zeros(b: &[u8]) -> &[u8] {
    let i = b
        .iter()
        .position(|&x| x != 0)
        .unwrap_or(b.len().saturating_sub(1));
    &b[i..]
}

impl Anchor {
    /// Trusts this CA certificate. Fails if it isn't an Ed25519 CA.
    pub fn new(ca_der: Vec<u8>) -> io::Result<Self> {
        let (_, c) = x509_parser::parse_x509_certificate(&ca_der).map_err(err)?;
        let ca_fp = fingerprint(c.public_key().raw);
        public_from_spki(c.public_key().raw).ok_or_else(|| err("site CA is not Ed25519"))?;
        Ok(Anchor {
            ca_der,
            ca_fp,
            crl: RwLock::new(Crl::default()),
        })
    }

    /// The CA's SubjectPublicKeyInfo (DER).
    pub fn ca_der_spki(&self) -> io::Result<Vec<u8>> {
        let (_, c) = x509_parser::parse_x509_certificate(&self.ca_der).map_err(err)?;
        Ok(c.public_key().raw.to_vec())
    }

    /// The first 16 hex digits of the CA fingerprint: `x_idn_ca` (§E.10.7).
    pub fn ca_id(&self) -> &str {
        &self.ca_fp[..16]
    }

    /// Verifies a CRL from the CA. Returns it if valid and newer than the one held, after
    /// installing it.
    pub fn accept_crl(&self, der: &[u8]) -> io::Result<Option<Crl>> {
        let (_, ca) = x509_parser::parse_x509_certificate(&self.ca_der).map_err(err)?;
        let (_, crl) = x509_parser::parse_x509_crl(der).map_err(err)?;
        crl.verify_signature(ca.public_key())
            .map_err(|e| err(format!("CRL signature: {e}")))?;
        let number = crl
            .crl_number()
            .and_then(|n| u64::try_from(n.clone()).ok())
            .ok_or_else(|| err("CRL without cRLNumber"))?;
        let mut cur = self.crl.write_ok();
        if number <= cur.number {
            return Ok(None);
        }
        let serials = crl
            .iter_revoked_certificates()
            .map(|r| strip_zeros(r.raw_serial()).to_vec())
            .collect();
        *cur = Crl {
            number,
            serials,
            der: der.to_vec(),
        };
        Ok(Some(cur.clone()))
    }

    /// Checks a machine's certificate for its HTTPS ports (§E.10.11): signed by this CA,
    /// for `public` (the machine's device key), for server authentication, not expired.
    pub fn verify_lan(&self, cert_der: &[u8], public: &[u8; 32]) -> io::Result<LanCert> {
        self.verify_lan_by(cert_der, |c| {
            public_from_spki(c.public_key().raw).as_ref() == Some(public)
        })
    }

    /// As [`Anchor::verify_lan`], for the machine whose device fingerprint is `machine`
    /// (`DeviceHealth.fingerprint`, the IDN Node's): its key, and its `urn:idn:lan:` name
    /// (a device certificate for `secure/2` is not one for HTTPS).
    pub fn verify_lan_of(&self, cert_der: &[u8], machine: &str) -> io::Result<LanCert> {
        let machine = machine.trim().to_ascii_lowercase();
        self.verify_lan_by(cert_der, |c| {
            let urn = format!("{LAN_URN}{machine}");
            fingerprint(c.public_key().raw) == machine
                && c.subject_alternative_name()
                    .ok()
                    .flatten()
                    .is_some_and(|san| {
                        san.value.general_names.iter().any(|n| {
                            matches!(n, x509_parser::extensions::GeneralName::URI(u)
                            if u.eq_ignore_ascii_case(&urn))
                        })
                    })
        })
    }

    fn verify_lan_by(
        &self,
        cert_der: &[u8],
        mine: impl FnOnce(&x509_parser::certificate::X509Certificate<'_>) -> bool,
    ) -> io::Result<LanCert> {
        let (_, ca) = x509_parser::parse_x509_certificate(&self.ca_der).map_err(err)?;
        let (_, c) = x509_parser::parse_x509_certificate(cert_der).map_err(err)?;
        c.verify_signature(Some(ca.public_key()))
            .map_err(|e| err(format!("not issued by the site CA: {e}")))?;
        if !mine(&c) {
            return Err(err("not for this machine's key"));
        }
        if !c
            .extended_key_usage()
            .ok()
            .flatten()
            .is_some_and(|e| e.value.server_auth)
        {
            return Err(err("not for server authentication"));
        }
        let mut out = LanCert {
            der: cert_der.to_vec(),
            names: Vec::new(),
            ips: Vec::new(),
            not_after: c.validity().not_after.timestamp(),
        };
        if let Ok(Some(san)) = c.subject_alternative_name() {
            for n in &san.value.general_names {
                match n {
                    x509_parser::extensions::GeneralName::DNSName(d) => {
                        out.names.push((*d).to_owned())
                    }
                    x509_parser::extensions::GeneralName::IPAddress(b) => {
                        if let Ok(v4) = <[u8; 4]>::try_from(*b) {
                            out.ips.push(std::net::IpAddr::from(v4));
                        } else if let Ok(v6) = <[u8; 16]>::try_from(*b) {
                            out.ips.push(std::net::IpAddr::from(v6));
                        }
                    }
                    _ => {}
                }
            }
        }
        if !out.valid_for(std::time::Duration::ZERO) {
            return Err(err("expired"));
        }
        Ok(out)
    }

    /// Checks a peer's certificate: signed by this CA, consistent fingerprint, not revoked.
    pub fn verify(&self, cert_der: &[u8]) -> io::Result<CertInfo> {
        self.verify_with(cert_der, true)
    }

    /// As [`Anchor::verify`]; without `check_crl`, revoked certificates pass (the
    /// authority uses this to tell a revoked device so, §E.10.6).
    pub fn verify_with(&self, cert_der: &[u8], check_crl: bool) -> io::Result<CertInfo> {
        let (_, ca) = x509_parser::parse_x509_certificate(&self.ca_der).map_err(err)?;
        let (_, c) = x509_parser::parse_x509_certificate(cert_der).map_err(err)?;
        c.verify_signature(Some(ca.public_key()))
            .map_err(|e| err(format!("not issued by the site CA: {e}")))?;
        let own_fp = fingerprint(c.public_key().raw);
        let mut info = CertInfo {
            serial: strip_zeros(c.raw_serial()).to_vec(),
            fingerprint: String::new(),
            name: c
                .subject()
                .iter_common_name()
                .next()
                .and_then(|n| n.as_str().ok())
                .unwrap_or("")
                .to_owned(),
            kind: String::new(),
            admin: false,
        };
        if let Ok(Some(san)) = c.subject_alternative_name() {
            for n in &san.value.general_names {
                if let x509_parser::extensions::GeneralName::URI(u) = n {
                    if let Some(fp) = u.strip_prefix("urn:idn:fp:") {
                        info.fingerprint = fp.to_owned();
                    } else if let Some(k) = u.strip_prefix("urn:idn:kind:") {
                        info.kind = k.to_owned();
                    } else if *u == "urn:idn:role:admin" {
                        info.admin = true;
                    }
                }
            }
        }
        if info.fingerprint != own_fp {
            return Err(err("certificate fingerprint does not match its key"));
        }
        if check_crl && self.crl.read_ok().serials.contains(&info.serial) {
            return Err(err(format!("certificate of {} is revoked", info.name)));
        }
        Ok(info)
    }
}

impl Anchor {
    /// A TLS client for machine `machine`'s HTTPS ports (its Node panel, the display
    /// service's 29223; §E.10.11): the site CA alone trusted, and the certificate must be
    /// that machine's (its device fingerprint, `DeviceHealth.fingerprint`), at whatever
    /// address it was reached (127.0.0.1, a changed IP): another site machine with a valid
    /// certificate of its own can't answer for it. TLS 1.3 with Ed25519 offered (else the
    /// port presents its own self-signed certificate); no client certificate.
    pub fn lan_client_config(self: &Arc<Self>, machine: &str) -> io::Result<rustls::ClientConfig> {
        let provider = crate::secure::crypto_provider();
        let v = LanVerifier {
            anchor: self.clone(),
            machine: machine.trim().to_ascii_lowercase(),
            algs: provider.signature_verification_algorithms,
        };
        Ok(rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(err)?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(v))
            .with_no_client_auth())
    }
}

/// Checks a machine's HTTPS certificate against the site CA and its fingerprint (the name
/// it was reached by doesn't matter: the fingerprint is the stronger check).
#[derive(Debug)]
struct LanVerifier {
    anchor: Arc<Anchor>,
    machine: String,
    algs: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for LanVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.anchor
            .verify_lan_of(end_entity.as_ref(), &self.machine)
            .map(|_| ServerCertVerified::assertion())
            .map_err(|e| rustls::Error::General(format!("not machine {}: {e}", self.machine)))
    }
    crate::secure::ed25519_tls13_signatures!(algs);
}

/// A device's TLS credentials for `secure/2` (§E.10.7): its key, its certificate, and
/// the anchor it checks peers against.
#[derive(Debug)]
pub struct DeviceCreds {
    /// The device key.
    pub key: KeyFile,
    /// Its certificate from the site CA.
    pub cert_der: Vec<u8>,
    /// The site CA and CRL.
    pub anchor: Arc<Anchor>,
}

impl DeviceCreds {
    /// TLS 1.3 client configuration, presenting the device certificate.
    pub fn client_config(self: &Arc<Self>, alpn: &[u8]) -> io::Result<rustls::ClientConfig> {
        self.client_config_for(alpn, None)
    }

    /// As [`DeviceCreds::client_config`], also requiring the server's certificate to be
    /// of `kind` (`urn:idn:kind:`): the trust connection talks only to the authority
    /// (§E.10.2), never to another enrolled device posing as it.
    pub fn client_config_for(
        self: &Arc<Self>,
        alpn: &[u8],
        kind: Option<&'static str>,
    ) -> io::Result<rustls::ClientConfig> {
        let provider = crate::secure::crypto_provider();
        let mut v = DeviceVerifier::new(self.anchor.clone(), &provider);
        v.server_kind = kind;
        let mut c = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(err)?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(v))
            .with_client_cert_resolver(
                self.key
                    .certified(vec![CertificateDer::from(self.cert_der.clone())], &provider)?,
            );
        c.alpn_protocols = vec![alpn.to_vec()];
        Ok(c)
    }

    /// TLS 1.3 server configuration, requiring a device certificate from the client.
    pub fn server_config(self: &Arc<Self>, alpn: &[u8]) -> io::Result<rustls::ServerConfig> {
        self.server_config_with(alpn, true)
    }

    /// As [`DeviceCreds::server_config`]; without `check_crl`, revoked clients complete
    /// the handshake (the authority's trust connection only).
    pub fn server_config_with(
        self: &Arc<Self>,
        alpn: &[u8],
        check_crl: bool,
    ) -> io::Result<rustls::ServerConfig> {
        let provider = crate::secure::crypto_provider();
        let mut v = DeviceVerifier::new(self.anchor.clone(), &provider);
        v.check_crl = check_crl;
        let mut c = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(err)?
            .with_client_cert_verifier(Arc::new(v))
            .with_cert_resolver(
                self.key
                    .certified(vec![CertificateDer::from(self.cert_der.clone())], &provider)?,
            );
        c.alpn_protocols = vec![alpn.to_vec()];
        Ok(c)
    }
}

/// Accepts certificates issued by the site CA and not revoked, in both directions.
#[derive(Debug)]
struct DeviceVerifier {
    anchor: Arc<Anchor>,
    algs: WebPkiSupportedAlgorithms,
    check_crl: bool,
    /// The kind a server's certificate must have, if any.
    server_kind: Option<&'static str>,
}

impl DeviceVerifier {
    fn new(anchor: Arc<Anchor>, provider: &CryptoProvider) -> Self {
        DeviceVerifier {
            anchor,
            algs: provider.signature_verification_algorithms,
            check_crl: true,
            server_kind: None,
        }
    }

    fn check(&self, end_entity: &CertificateDer<'_>) -> Result<CertInfo, rustls::Error> {
        self.anchor
            .verify_with(end_entity.as_ref(), self.check_crl)
            .map_err(|e| rustls::Error::General(e.to_string()))
    }
}

impl ServerCertVerifier for DeviceVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let info = self.check(end_entity)?;
        match self.server_kind {
            Some(k) if info.kind != k => Err(rustls::Error::General(format!(
                "the server is \"{}\" ({}), not the site {k}",
                info.name, info.kind
            ))),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }
    crate::secure::ed25519_tls13_signatures!(algs);
}

impl ClientCertVerifier for DeviceVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|_| ClientCertVerified::assertion())
    }
    crate::secure::ed25519_tls13_signatures!(algs);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (std::net::TcpStream, std::net::TcpStream) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let c = std::net::TcpStream::connect(l.local_addr().unwrap()).unwrap();
        (c, l.accept().unwrap().0)
    }

    #[test]
    fn lan_certificates_name_the_machine_and_serve_https_only() {
        let ca = Ca::from_key(KeyFile::generate().unwrap()).unwrap();
        let anchor = Anchor::new(ca.cert_der()).unwrap();
        let dev = KeyFile::generate().unwrap();
        let ip: std::net::IpAddr = "192.168.1.20".parse().unwrap();
        let der = ca
            .issue_lan(
                &dev.spki(),
                "BOOTH",
                &["booth".into(), "localhost".into()],
                &[ip],
            )
            .unwrap();
        let c = anchor.verify_lan(&der, &dev.public).unwrap();
        assert_eq!(c.names, ["booth", "localhost"]);
        assert_eq!(c.ips, [ip]);
        assert!(c.valid_for(LAN_CERT_LIFE / 2));
        assert!(!c.valid_for(LAN_CERT_LIFE));
        // Another key's, or another site's: refused.
        assert!(
            anchor
                .verify_lan(&der, &KeyFile::generate().unwrap().public)
                .is_err()
        );
        let other = Anchor::new(
            Ca::from_key(KeyFile::generate().unwrap())
                .unwrap()
                .cert_der(),
        )
        .unwrap();
        assert!(other.verify_lan(&der, &dev.public).is_err());
        // Never a device certificate for secure/2.
        assert!(anchor.verify(&der).is_err());
        // A device certificate isn't one for HTTPS either (it carries no names): it
        // passes this check (server authentication) but names nothing.
        let (dev_der, _) = ca.issue(&dev.spki(), "BOOTH", "app", false).unwrap();
        let d = anchor.verify_lan(&dev_der, &dev.public).unwrap();
        assert!(d.names.is_empty() && d.ips.is_empty());
        // By the machine's fingerprint (a pinned client): its LAN certificate
        // only, not its secure/2 one, nor another machine's.
        assert!(anchor.verify_lan_of(&der, &dev.fingerprint()).is_ok());
        assert!(
            anchor
                .verify_lan_of(&der, &dev.fingerprint().to_ascii_uppercase())
                .is_ok()
        );
        assert!(anchor.verify_lan_of(&dev_der, &dev.fingerprint()).is_err());
        assert!(
            anchor
                .verify_lan_of(&der, &KeyFile::generate().unwrap().fingerprint())
                .is_err()
        );
        assert!(other.verify_lan_of(&der, &dev.fingerprint()).is_err());
        let p = pem("CERTIFICATE", &ca.cert_der());
        assert!(p.starts_with("-----BEGIN CERTIFICATE-----\n"));
        assert!(p.lines().all(|l| l.len() <= 64));
    }

    #[test]
    fn issue_verify_and_revoke() {
        let ca = Ca::from_key(KeyFile::generate().unwrap()).unwrap();
        // The CA certificate is the same each time it is rebuilt from the key.
        let again = Ca::from_key(KeyFile::from_pkcs8(ca.key().pkcs8().to_vec()).unwrap()).unwrap();
        assert_eq!(ca.cert_der(), again.cert_der());

        let dev = KeyFile::generate().unwrap();
        let (der, info) = ca
            .issue(&dev.spki(), "SANCTUARY (Program)", "app", true)
            .unwrap();
        let anchor = Anchor::new(ca.cert_der()).unwrap();
        let got = anchor.verify(&der).unwrap();
        assert_eq!(got, info);
        assert!(got.admin && got.kind == "app" && got.fingerprint == dev.fingerprint());

        // A certificate from another CA is refused.
        let other = Ca::from_key(KeyFile::generate().unwrap()).unwrap();
        let (bad, _) = other.issue(&dev.spki(), "X", "app", false).unwrap();
        assert!(anchor.verify(&bad).is_err());

        // Revocation by CRL; older or equal numbers are ignored; forged CRLs refused.
        let crl1 = ca.crl(1, std::slice::from_ref(&info.serial)).unwrap();
        assert!(anchor.accept_crl(&crl1).unwrap().is_some());
        assert!(anchor.verify(&der).is_err());
        assert!(
            anchor
                .accept_crl(&ca.crl(1, &[]).unwrap())
                .unwrap()
                .is_none(),
            "same number ignored"
        );
        assert!(
            anchor.accept_crl(&other.crl(9, &[]).unwrap()).is_err(),
            "foreign CRL"
        );
        assert!(
            anchor
                .accept_crl(&ca.crl(2, &[]).unwrap())
                .unwrap()
                .is_some()
        );
        assert!(anchor.verify(&der).is_ok(), "un-revoked by a newer CRL");
    }

    #[test]
    fn mutual_tls_with_device_certificates() {
        let ca = Ca::from_key(KeyFile::generate().unwrap()).unwrap();
        let creds = |name: &str| {
            let k = KeyFile::generate().unwrap();
            let (der, _) = ca.issue(&k.spki(), name, "app", false).unwrap();
            Arc::new(DeviceCreds {
                key: k,
                cert_der: der,
                anchor: Arc::new(Anchor::new(ca.cert_der()).unwrap()),
            })
        };
        let (a, b) = (creds("A"), creds("B"));
        let (c, s) = pair();
        let srv = std::thread::spawn(move || {
            let conn =
                rustls::ServerConnection::new(Arc::new(b.server_config(b"t").unwrap())).unwrap();
            crate::secure::wrap(s, rustls::Connection::Server(conn)).map(|_| ())
        });
        let conn = rustls::ClientConnection::new(
            Arc::new(a.client_config(b"t").unwrap()),
            "idn-site".try_into().unwrap(),
        )
        .unwrap();
        assert!(crate::secure::wrap(c, rustls::Connection::Client(conn)).is_ok());
        assert!(srv.join().unwrap().is_ok());

        // A device from another site is refused by the server.
        let other_ca = Ca::from_key(KeyFile::generate().unwrap()).unwrap();
        let k = KeyFile::generate().unwrap();
        let (der, _) = other_ca.issue(&k.spki(), "EVIL", "app", false).unwrap();
        let evil = Arc::new(DeviceCreds {
            key: k,
            cert_der: der,
            anchor: Arc::new(Anchor::new(other_ca.cert_der()).unwrap()),
        });
        let good = creds("C");
        let (c, s) = pair();
        let srv = std::thread::spawn(move || {
            let conn =
                rustls::ServerConnection::new(Arc::new(good.server_config(b"t").unwrap())).unwrap();
            crate::secure::wrap(s, rustls::Connection::Server(conn)).map(|_| ())
        });
        let conn = rustls::ClientConnection::new(
            Arc::new(evil.client_config(b"t").unwrap()),
            "idn-site".try_into().unwrap(),
        )
        .unwrap();
        let _ = crate::secure::wrap(c, rustls::Connection::Client(conn));
        assert!(srv.join().unwrap().is_err());
    }

    #[test]
    fn the_trust_connection_only_accepts_the_authority() {
        let ca = Ca::from_key(KeyFile::generate().unwrap()).unwrap();
        let creds = |name: &str, kind: &str| {
            let k = KeyFile::generate().unwrap();
            let (der, _) = ca.issue(&k.spki(), name, kind, false).unwrap();
            Arc::new(DeviceCreds {
                key: k,
                cert_der: der,
                anchor: Arc::new(Anchor::new(ca.cert_der()).unwrap()),
            })
        };
        let device = creds("Stage", "output");
        for (server, ok) in [
            (creds("Lobby", "app"), false),
            (creds("IDN site authority", "authority"), true),
        ] {
            let (c, s) = pair();
            let srv = std::thread::spawn(move || {
                let conn =
                    rustls::ServerConnection::new(Arc::new(server.server_config(b"t").unwrap()))
                        .unwrap();
                let _ = crate::secure::wrap(s, rustls::Connection::Server(conn));
            });
            let conn = rustls::ClientConnection::new(
                Arc::new(device.client_config_for(b"t", Some("authority")).unwrap()),
                "idn-site".try_into().unwrap(),
            )
            .unwrap();
            let r = crate::secure::wrap(c, rustls::Connection::Client(conn));
            assert_eq!(r.is_ok(), ok, "{:?}", r.err());
            drop(r);
            srv.join().unwrap();
        }
    }

    #[test]
    fn files_differing_only_by_extension_never_swap_contents() {
        let dir = idn_common::testing::TempDir::new("pki-tmp");
        let threads: Vec<_> = ["key", "crt"]
            .into_iter()
            .map(|ext| {
                let p = dir.join(format!("server.{ext}"));
                std::thread::spawn(move || {
                    for i in 0..200 {
                        write_private(&p, format!("{ext} {i}").as_bytes()).unwrap();
                        let got = std::fs::read_to_string(&p).unwrap();
                        assert!(got.starts_with(ext), "server.{ext} holds {got:?}");
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(left.len(), 2, "no temporary files left behind");
    }
}
