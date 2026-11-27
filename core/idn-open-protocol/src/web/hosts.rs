//! Opening the browser, the panel's certificate, and the Host names a panel answers to.

use super::*;

/// Opens `url` in this machine's default browser; false if that failed (say where to go
/// instead).
pub fn open_browser(url: &str) -> bool {
    let r = if cfg!(windows) {
        // Not through cmd: there `&` in an address would start a command.
        std::process::Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", url])
            .status()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).status()
    } else {
        // The system's xdg-open by its full path, without a `BROWSER` or library
        // preloading handed down: a program waiting for approval ran `idn-session --open`
        // with its own `xdg-open` first on PATH and caught the panel's address (bug run
        // 2026-11-25).
        let xdg = [
            "/usr/bin/xdg-open",
            "/bin/xdg-open",
            "/usr/local/bin/xdg-open",
        ]
        .into_iter()
        .find(|p| std::path::Path::new(p).is_file())
        .unwrap_or("xdg-open");
        std::process::Command::new(xdg)
            .arg(url)
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env_remove("BROWSER")
            .env_remove("LD_PRELOAD")
            .env_remove("LD_LIBRARY_PATH")
            .env_remove("LD_AUDIT")
            .status()
    };
    r.is_ok_and(|s| s.success())
}

/// For a program with a panel started with `--open` (a Start-menu entry): if one already
/// runs on `port`, opens its panel and exits; otherwise returns and the program starts.
pub fn open_existing(port: u16) {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    if std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(300)).is_ok() {
        let url = format!("http://127.0.0.1:{port}/");
        if !open_browser(&url) {
            idn_common::log::log_or_stderr(&format!("already running: open {url}"));
        }
        std::process::exit(0);
    }
}

/// A self-signed certificate for the panel, made once and kept in `dir`
/// (`panel-p256.key`, `panel-p256.crt`, ECDSA P-256): the TLS configuration and the
/// certificate's SHA-256 fingerprint (to compare with what the browser shows).
pub fn self_signed(dir: &Path, name: &str) -> io::Result<(Arc<rustls::ServerConfig>, String)> {
    let (cert, key_der, fp) = panel_cert(dir, name)?;
    let cfg = rustls::ServerConfig::builder_with_provider(crate::secure::crypto_provider())
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(io::Error::other)?
        .with_no_client_auth()
        .with_single_cert(
            vec![rustls::pki_types::CertificateDer::from(cert)],
            rustls::pki_types::PrivateKeyDer::Pkcs8(key_der.into()),
        )
        .map_err(io::Error::other)?;
    Ok((Arc::new(cfg), fp))
}

/// The panel's self-signed certificate ([`self_signed`]), made the first time: the
/// certificate (DER), its key (PKCS#8) and its SHA-256 fingerprint.
pub(crate) fn panel_cert(dir: &Path, name: &str) -> io::Result<(Vec<u8>, Vec<u8>, String)> {
    // ECDSA P-256, which every browser accepts: Chromium (Chrome, Edge) doesn't offer
    // Ed25519 in TLS at all (lab), so the machine's Ed25519 key can't serve
    // here. The panels have their own key.
    let key_path = dir.join("panel-p256.key");
    let crt_path = dir.join("panel-p256.crt");
    let key_der = match std::fs::read(&key_path) {
        Ok(k) => k,
        Err(_) => {
            let kp = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
                .map_err(io::Error::other)?;
            let der = kp.serialize_der();
            crate::trust::pki::write_private(&key_path, &der)?;
            let _ = std::fs::remove_file(&crt_path);
            der
        }
    };
    let cert = match std::fs::read(&crt_path) {
        Ok(c) => c,
        Err(_) => {
            let kp = rcgen::KeyPair::try_from(key_der.as_slice()).map_err(io::Error::other)?;
            let host = crate::discovery::hostname().to_lowercase();
            let mut p = rcgen::CertificateParams::new(vec![
                host.clone(),
                format!("{host}.local"),
                "localhost".to_owned(),
            ])
            .map_err(io::Error::other)?;
            p.distinguished_name = rcgen::DistinguishedName::new();
            p.distinguished_name.push(rcgen::DnType::CommonName, name);
            p.not_before = rcgen::date_time_ymd(2026, 1, 1);
            p.not_after = rcgen::date_time_ymd(2046, 1, 1);
            let c = p.self_signed(&kp).map_err(io::Error::other)?.der().to_vec();
            crate::trust::pki::write_private(&crt_path, &c)?;
            c
        }
    };
    let fp = idn_common::hex::hex_upper_colons(&crate::hash::sha256(&cert));
    Ok((cert, key_der, fp))
}

/// A Host header a page open to the network without a password may answer (DNS
/// rebinding: a web site whose name was pointed at this machine's address would be its
/// own origin to the browser, and read the page and its pictures; deep bug run
/// 2026-11-09): an address, `localhost`, a `.local` name, a name with no dots, this
/// machine's name in one of the system's search domains (`/etc/resolv.conf`; not any
/// domain: `booth.attacker.example` passed for a machine named `booth`, bug run
/// 2026-11-24), or a name listed in `IDN_VIEW_HOSTS` (comma-separated).
pub fn lan_host_ok(host: Option<&str>) -> bool {
    let Some(h) = host.map(|h| h.trim().to_ascii_lowercase()) else {
        return false;
    };
    let (name, _) = split_host(&h);
    // In brackets: an IPv6 address only.
    if h.starts_with('[') {
        return name.parse::<std::net::Ipv6Addr>().is_ok();
    }
    let name = name.trim_end_matches('.');
    if name.parse::<std::net::IpAddr>().is_ok()
        || is_loopback_host(name)
        || !name.contains('.')
        || name.ends_with(".local")
    {
        return true;
    }
    let me = crate::discovery::hostname().to_ascii_lowercase();
    if let Some(domain) = name
        .strip_prefix(me.as_str())
        .and_then(|r| r.strip_prefix('.'))
        && search_domains().iter().any(|d| d == domain)
    {
        return true;
    }
    std::env::var("IDN_VIEW_HOSTS")
        .unwrap_or_default()
        .split(',')
        .any(|n| n.trim().trim_end_matches('.').eq_ignore_ascii_case(name))
}

/// The system's DNS search domains (`search` and `domain` in `/etc/resolv.conf`),
/// lowercase; none elsewhere.
fn search_domains() -> Vec<String> {
    std::fs::read_to_string("/etc/resolv.conf")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            matches!(w.next(), Some("search" | "domain")).then(|| {
                w.map(|d| d.trim_end_matches('.').to_ascii_lowercase())
                    .collect::<Vec<_>>()
            })
        })
        .flatten()
        .collect()
}

/// A Host header naming this machine's loopback ([`is_loopback_name`]) at `port`.
pub fn loopback_host(host: Option<&str>, port: u16) -> bool {
    host.map(split_host)
        .is_some_and(|(name, p)| p == Some(port) && is_loopback_host(name))
}
