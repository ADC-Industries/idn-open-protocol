//! A program's panel on the network: HTTPS, a password or a site token, then the same
//! requests as on the machine itself (the switcher's and the mixer's, which were the
//! same code twice).
//!
//! The certificate: on a machine in a site, the machine's certificate for its HTTPS ports
//! signed by the site CA (idn-extensions.md §E.10.11), with the CA after it, so a client
//! pins the site CA; else (and for a client that can't take Ed25519, a browser) the
//! program's own self-signed one. Looked at on each connection: joining, leaving and
//! renewals apply without a restart.

use super::{Handler, PasswordGate, Request, Response, Who};
use crate::trust::Enrollment;
use idn_common::sync::{LockOk as _, RwLockOk as _};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{CipherSuite, SignatureScheme};
use std::io;
use std::net::TcpListener;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, RwLock, Weak};

/// The machine's site membership now, for a network port's certificate (`None`: not in a
/// site, or none known here).
pub type Membership = Arc<dyn Fn() -> Option<Arc<Enrollment>> + Send + Sync>;

/// The membership of this program's connection to its IDN Node (the last made).
static PROCESS: RwLock<Option<Weak<Enrollment>>> = RwLock::new(None);

/// Makes `e` this program's membership for [`process_membership`] (done by
/// [`crate::node::Node::connect`]).
pub fn use_membership(e: &Arc<Enrollment>) {
    *PROCESS.write_ok() = Some(Arc::downgrade(e));
}

/// This program's membership as its IDN Node holds it: what [`start`] and
/// [`start_when`] present the site's certificate from.
pub fn process_membership() -> Membership {
    Arc::new(|| PROCESS.read_ok().as_ref()?.upgrade())
}

/// The TLS configuration of a network port: the site's certificate for the machine while
/// `site` gives an enrolled membership holding one (§E.10.11), else the self-signed one
/// kept in `dir` for `name` ([`super::self_signed`]), whose fingerprint comes with it.
pub fn server_config(
    dir: &Path,
    name: &str,
    site: Membership,
) -> io::Result<(Arc<rustls::ServerConfig>, String)> {
    let (cert, key, fp) = super::panel_cert(dir, name)?;
    let provider = crate::secure::crypto_provider();
    let own_key = provider
        .key_provider
        .load_private_key(rustls::pki_types::PrivateKeyDer::Pkcs8(key.into()))
        .map_err(io::Error::other)?;
    let own = Arc::new(CertifiedKey::new(
        vec![rustls::pki_types::CertificateDer::from(cert)],
        own_key,
    ));
    let resolver = SiteOrOwn {
        own,
        site,
        provider: provider.clone(),
        made: Mutex::new(None),
    };
    let cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(io::Error::other)?
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(resolver));
    Ok((Arc::new(cfg), fp))
}

/// The certificate for each connection ([`server_config`]).
struct SiteOrOwn {
    own: Arc<CertifiedKey>,
    site: Membership,
    provider: Arc<rustls::crypto::CryptoProvider>,
    /// The site's chain as last made.
    made: Mutex<Option<Made>>,
}

/// A site chain made: for which membership (its address) and certificate.
type Made = (usize, Vec<u8>, Arc<CertifiedKey>);

impl std::fmt::Debug for SiteOrOwn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SiteOrOwn")
    }
}

impl SiteOrOwn {
    /// The site's chain for the machine, now: its certificate and the CA, with the device
    /// key (signed for by the IDN Node when it holds the key).
    fn site_chain(&self) -> Option<Arc<CertifiedKey>> {
        let e = (self.site)()?;
        let creds = e.creds()?;
        let lan = e.lan_cert()?;
        let id = Arc::as_ptr(&e) as usize;
        let mut made = self.made.lock_ok();
        if let Some((i, der, ck)) = &*made
            && *i == id
            && *der == lan.der
        {
            return Some(ck.clone());
        }
        let key = e.device_key().signing_key(&self.provider).ok()?;
        let ck = Arc::new(CertifiedKey::new(
            vec![
                rustls::pki_types::CertificateDer::from(lan.der.clone()),
                rustls::pki_types::CertificateDer::from(creds.anchor.ca_der.clone()),
            ],
            key,
        ));
        *made = Some((id, lan.der.clone(), ck.clone()));
        Some(ck)
    }
}

impl ResolvesServerCert for SiteOrOwn {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        // The site's key is Ed25519, and when the IDN Node holds it, it signs TLS 1.3
        // handshakes only: a client offering neither (a browser) gets the program's own.
        let ed25519 = hello
            .signature_schemes()
            .contains(&SignatureScheme::ED25519);
        let tls13 = hello.cipher_suites().iter().any(|c| {
            matches!(
                c,
                CipherSuite::TLS13_AES_128_GCM_SHA256
                    | CipherSuite::TLS13_AES_256_GCM_SHA384
                    | CipherSuite::TLS13_CHACHA20_POLY1305_SHA256
            )
        });
        if ed25519
            && tls13
            && let Some(ck) = self.site_chain()
        {
            return Some(ck);
        }
        Some(self.own.clone())
    }
}

/// The password given as `--lan-password VALUE`: `-` reads it from standard input (never
/// on the command line itself: process listings show it).
pub fn password_arg(value: &str) -> Result<String, String> {
    if value != "-" {
        return Err("--lan-password takes - (read it from standard input)".into());
    }
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

/// A request from the network: the gate's own requests (login, logout), the shared files
/// and the login page before logging in, `then` once logged in or with a valid
/// site-signed token, given who sent it (idn-node.md §N.31.5). `log` hears the gate's
/// messages (logins and tokens refused).
pub fn front(
    gate: Option<&PasswordGate>,
    login: &super::ui::Login,
    r: Request,
    log: &dyn Fn(&str),
    then: impl FnOnce(Request, Who) -> Response,
) -> Response {
    let Some(gate) = gate else {
        return Response::json_err("404 Not Found", "no such page");
    };
    if let Some(resp) = gate.handle(&r, log) {
        return resp;
    }
    if r.method == "GET"
        && r.path.starts_with("/ui/")
        && let Some(a) = super::ui::asset(&r.path)
    {
        // The shared files, before logging in (nothing secret in them).
        return a;
    }
    match gate.who(&r) {
        Ok(who) => {
            // A WebSocket goes on only while its session (or token) does (bug run O2-M3).
            let still = gate.still_allowed_for(&r, &who);
            then(r, who).while_allowed(still)
        }
        Err(_) if r.method == "GET" && r.path == "/" && gate.has_password() => {
            Response::ok("text/html; charset=utf-8", super::ui::login_page(login))
        }
        Err(resp) => {
            if resp.status.starts_with("401") {
                log(&format!(
                    "token refused from {}: {}",
                    r.peer,
                    String::from_utf8_lossy(&resp.body)
                ));
            }
            resp
        }
    }
}

/// Starts the network port with [`start`] once `want` says so (a password set, or the
/// machine in a site so that tokens can be checked), looking every few seconds until
/// then; `say` hears where it is, or why not.
pub fn start_when(
    dir: &Path,
    name: &'static str,
    port: u16,
    stop: Arc<AtomicBool>,
    handler: Handler,
    want: impl Fn() -> bool + Send + 'static,
    say: impl Fn(&str) + Send + 'static,
) {
    let dir = dir.to_owned();
    let _ = std::thread::Builder::new()
        .name("idn-lan-start".into())
        .spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                if want() {
                    match start(&dir, name, port, stop.clone(), handler) {
                        Ok(fp) => say(&format!(
                            "on the network at https://<this machine>:{port}/ (in a site: the site's certificate; else this one, {fp})"
                        )),
                        Err(e) => say(&format!("not on the network: {e}")),
                    }
                    return;
                }
                std::thread::sleep(std::time::Duration::from_secs(3));
            }
        });
}

/// Serves `handler` on `port` over HTTPS until `stop`, with the site's certificate while
/// this program's IDN Node has the machine in a site ([`process_membership`]), else the
/// self-signed one kept in `dir` for `name` (made the first time): its fingerprint, to
/// print.
pub fn start(
    dir: &Path,
    name: &str,
    port: u16,
    stop: Arc<AtomicBool>,
    handler: Handler,
) -> io::Result<String> {
    start_for(dir, name, port, stop, handler, process_membership())
}

/// [`start`] with the site's certificate from `site` ([`server_config`]).
pub fn start_for(
    dir: &Path,
    name: &str,
    port: u16,
    stop: Arc<AtomicBool>,
    handler: Handler,
    site: Membership,
) -> io::Result<String> {
    std::fs::create_dir_all(dir)?;
    let (tls, fp) = server_config(dir, name, site)?;
    let l = TcpListener::bind(("0.0.0.0", port))?;
    super::serve(l, Some(tls), handler, stop)?;
    Ok(fp)
}
