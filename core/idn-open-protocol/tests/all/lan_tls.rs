//! A machine's certificate for its HTTPS ports (idn-extensions.md §E.10.11): issued by the
//! site over the trust connection, presented by a network port (`web::lan`) with the CA
//! after it so a client pinning only the site CA connects by address and by name; the
//! port's own self-signed certificate outside a site (and for a client without Ed25519);
//! joining, a restart and leaving taken without restarting the port.

use crate::common::{Site, site, wait_connected};
use idn_common::sync::LockOk;
use idn_common::testing::wait_for;
use idn_open_protocol::secure::crypto_provider;
use idn_open_protocol::trust::Enrollment;
use idn_open_protocol::web::{self, Handler, Request, Response};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use std::io::{Read, Write};
use std::net::{IpAddr, TcpStream};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The machine, its state in the site's folder (kept across its restart).
fn device(site: &Site) -> Arc<Enrollment> {
    crate::common::device(
        &site.server,
        &site.dir.join("device"),
        "BOOTH (App)",
        site.passphrase.as_deref(),
    )
}

/// Takes any certificate, keeping the one presented; offers `schemes`.
#[derive(Debug)]
struct Any {
    provider: Arc<rustls::crypto::CryptoProvider>,
    schemes: Vec<SignatureScheme>,
}

impl ServerCertVerifier for Any {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            m,
            c,
            d,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            m,
            c,
            d,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes.clone()
    }
}

/// A GET over TLS to `ip:port` as `name`: the certificate the server presented, or why
/// not.
fn get(cfg: rustls::ClientConfig, ip: IpAddr, port: u16, name: &str) -> Result<Vec<u8>, String> {
    let conn = rustls::ClientConnection::new(
        Arc::new(cfg),
        ServerName::try_from(name.to_owned()).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let tcp = TcpStream::connect((ip, port)).map_err(|e| e.to_string())?;
    tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut s = rustls::StreamOwned::new(conn, tcp);
    s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let text = String::from_utf8_lossy(&out);
    if !text.starts_with("HTTP/1.1 200") {
        return Err(format!("no answer: {text:?}"));
    }
    Ok(s.conn.peer_certificates().unwrap()[0].to_vec())
}

/// A client trusting the site CA alone, names checked as usual.
fn pinned(ca: &[u8]) -> rustls::ClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(ca.to_vec())).unwrap();
    rustls::ClientConfig::builder_with_provider(crypto_provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth()
}

/// A client taking anything, offering `schemes`.
fn any(schemes: Vec<SignatureScheme>) -> rustls::ClientConfig {
    let p = crypto_provider();
    rustls::ClientConfig::builder_with_provider(p.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Any {
            provider: p,
            schemes,
        }))
        .with_no_client_auth()
}

fn all_schemes() -> Vec<SignatureScheme> {
    crypto_provider()
        .signature_verification_algorithms
        .supported_schemes()
}

#[test]
fn a_network_port_presents_the_site_certificate_while_the_machine_is_in_the_site() {
    let site = site("lan-tls", Some("St Mark"));
    let authority = &site.authority;
    let ca = authority.ca_der();
    assert!(
        authority
            .ca_pem()
            .starts_with("-----BEGIN CERTIFICATE-----\n")
    );

    // A network port on a machine not in a site yet: its own certificate.
    let dir = site.dir.join("port");
    std::fs::create_dir_all(&dir).unwrap();
    let member: Arc<Mutex<Option<Arc<Enrollment>>>> = Arc::default();
    let membership: web::lan::Membership = {
        let m = member.clone();
        Arc::new(move || m.lock_ok().clone())
    };
    let (tls, _) = web::lan::server_config(&dir, "Test", membership).unwrap();
    let own = std::fs::read(dir.join("panel-p256.crt")).unwrap();
    let handler: Handler = Arc::new(|_r: Request| Response::text("200 OK", "ok"));
    let l = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let stop = Arc::new(AtomicBool::new(false));
    web::serve(l, Some(tls), handler, stop.clone()).unwrap();
    let lo: IpAddr = "127.0.0.1".parse().unwrap();
    assert_eq!(get(any(all_schemes()), lo, port, "localhost").unwrap(), own);
    assert!(get(pinned(&ca), lo, port, "localhost").is_err());

    // It joins: the site issues its certificate for the HTTPS ports.
    let sdir = site.dir.join("device");
    let d = device(&site);
    wait_connected(&d);
    wait_for(
        "the site's HTTPS certificate",
        Duration::from_secs(10),
        || d.lan_cert().is_some(),
    );
    let lan = d.lan_cert().unwrap();
    let host = idn_open_protocol::discovery::hostname().to_ascii_lowercase();
    assert!(lan.names.contains(&"localhost".to_owned()), "{lan:?}");
    assert!(lan.ips.contains(&lo), "{lan:?}");
    // Not a device certificate: secure/2 doesn't take it.
    let creds = d.creds().unwrap();
    assert!(creds.anchor.verify(&lan.der).is_err());
    assert!(sdir.join("lan.crt").exists());

    // The same port, not restarted, presents it with the CA after it: a client pinning
    // only the site CA connects by address and by name.
    *member.lock_ok() = Some(d.clone());
    assert_eq!(get(pinned(&ca), lo, port, "127.0.0.1").unwrap(), lan.der);
    assert_eq!(get(pinned(&ca), lo, port, "localhost").unwrap(), lan.der);
    if lan.names.contains(&host) {
        assert_eq!(get(pinned(&ca), lo, port, &host).unwrap(), lan.der);
    }
    // ...and by its LAN address, when it has one.
    if let Some(ip) = lan.ips.iter().find(|i| !i.is_loopback()) {
        assert_eq!(
            get(pinned(&ca), *ip, port, &ip.to_string()).unwrap(),
            lan.der
        );
    }
    // A name it doesn't carry is refused.
    assert!(get(pinned(&ca), lo, port, "elsewhere.example").is_err());
    // Pinned to the machine: any name passes, another machine's doesn't.
    let pin = |fp: &str| authority.lan_client_config(fp).unwrap();
    assert_eq!(
        get(pin(&d.fingerprint()), lo, port, "elsewhere.example").unwrap(),
        lan.der
    );
    assert!(get(pin(&"0".repeat(64)), lo, port, "127.0.0.1").is_err());
    // A client without Ed25519 (a browser) gets the port's own.
    let no_ed: Vec<_> = all_schemes()
        .into_iter()
        .filter(|s| *s != SignatureScheme::ED25519)
        .collect();
    assert_eq!(get(any(no_ed), lo, port, "localhost").unwrap(), own);

    // Started again: the certificate kept is used at once.
    drop(d);
    *member.lock_ok() = None;
    std::thread::sleep(Duration::from_millis(300));
    let d = device(&site);
    assert_eq!(d.lan_cert().map(|c| c.der.clone()), Some(lan.der.clone()));
    *member.lock_ok() = Some(d.clone());
    assert_eq!(get(pinned(&ca), lo, port, "127.0.0.1").unwrap(), lan.der);

    // Revoked: out of the site, the port's own certificate again.
    wait_connected(&d);
    authority.revoke(&d.fingerprint()).unwrap();
    wait_for("the revocation", Duration::from_secs(10), || {
        d.creds().is_none()
    });
    assert!(d.lan_cert().is_none());
    assert!(!sdir.join("lan.crt").exists());
    assert_eq!(get(any(all_schemes()), lo, port, "localhost").unwrap(), own);
    assert!(get(pinned(&ca), lo, port, "127.0.0.1").is_err());
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
}
