//! What the app keeps (§V.11): the Voice server, the talk key, whether it starts at log-in,
//! and the self-signed certificates the user trusted, each for its server (host:port).
//! A JSON file in the user's settings folder.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// The Voice page (`https://host:29203/voice/`); empty: not set up yet.
    #[serde(default)]
    pub server: String,
    /// The talk key or foot pedal (Windows virtual-key code), and its name as shown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<u32>,
    #[serde(default)]
    pub key_name: String,
    /// The talk key also reaches the program in front (off: IDN Voice takes it).
    #[serde(default)]
    pub pass_key: bool,
    /// Starts at log-in, in the background (the tray icon; Linux: the window minimised).
    #[serde(default)]
    pub autostart: bool,
    /// "host:port" → the SHA-256 of the certificate trusted there (`AB:CD:…`).
    #[serde(default)]
    pub trusted: BTreeMap<String, String>,
    /// The first close was told that the app keeps running in the tray.
    #[serde(default)]
    pub told_tray: bool,
}

/// The settings file: `%APPDATA%\IDN\voice-app.json`, `~/.config/idn/voice-app.json`.
pub fn path() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("IDN").join("voice-app.json"))
    } else {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("idn").join("voice-app.json"))
    }
}

impl Settings {
    /// The saved settings, or the defaults when there are none. A damaged file is set
    /// aside (`voice-app.json.bad-SECS-PID-N`) and said, never read as "none" and then saved over.
    pub fn load() -> Settings {
        path().map_or_else(Settings::default, |p| Settings::load_from(&p))
    }

    fn load_from(p: &Path) -> Settings {
        match idn_common::fsutil::load_json(p) {
            Ok(s) => s.unwrap_or_default(),
            Err(e) => {
                // (Into the log: diag tees this program's errors there.)
                eprintln!("idn-voice: settings: {e}");
                Settings::default()
            }
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let p = path().ok_or_else(|| idn_common::err::other("no settings folder"))?;
        // Whole or not at all: a crash never leaves half a file.
        idn_common::fsutil::write(p, serde_json::to_vec_pretty(self).unwrap_or_default())
    }
}

/// The Voice page for what the user typed: a full link as it is; a bare name or address
/// the Media Site Manager's Voice (`https://NAME:29203/voice/`); a scheme added when
/// missing (https), and `/voice/` when no path is given. `None`: not an address.
pub fn server_url(typed: &str) -> Option<String> {
    let t = typed.trim();
    if t.is_empty() || t.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let (scheme, rest) = match t.split_once("://") {
        Some((s, r)) => (s.to_ascii_lowercase(), r),
        None => ("https".to_owned(), t),
    };
    if scheme != "https" && scheme != "http" {
        return None;
    }
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if hostport.is_empty() || hostport.contains('@') {
        return None;
    }
    let has_port = if hostport.starts_with('[') {
        hostport.contains("]:")
    } else {
        hostport.contains(':')
    };
    let hostport = if has_port || t.contains("://") {
        hostport.to_owned()
    } else {
        format!("{hostport}:29203")
    };
    let path = match path {
        "" | "/" => "/voice/".to_owned(),
        p if p.ends_with('/') => p.to_owned(),
        p => format!("{p}/"),
    };
    Some(format!("{scheme}://{hostport}{path}"))
}

/// `scheme://host:port` of a link (the default port written out), for comparing origins.
pub fn origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let hostport = rest.split(['/', '?', '#']).next()?.to_ascii_lowercase();
    if hostport.is_empty() {
        return None;
    }
    let has_port = if hostport.starts_with('[') {
        hostport.contains("]:")
    } else {
        hostport.contains(':')
    };
    let hostport = if has_port {
        hostport
    } else {
        let port = match scheme.as_str() {
            "https" | "wss" => 443,
            _ => 80,
        };
        format!("{hostport}:{port}")
    };
    Some(format!("{scheme}://{hostport}"))
}

/// `host:port` of a link, the key certificates are trusted under.
pub fn host_port(url: &str) -> Option<String> {
    origin(url).and_then(|o| o.split_once("://").map(|(_, h)| h.to_owned()))
}

/// Whether `url` is on the Voice server `server` (same scheme, host and port).
pub fn same_origin(url: &str, server: &str) -> bool {
    matches!((origin(url), origin(server)), (Some(a), Some(b)) if a == b)
}

/// The SHA-256 of a certificate (DER), as the Site Manager shows it: `AB:CD:…`.
pub fn fingerprint(der: &[u8]) -> String {
    idn_common::hex::hex_upper_colons(&idn_open_protocol::hash::sha256(der))
}

/// The first certificate of a PEM text, as DER.
pub fn pem_der(pem: &str) -> Option<Vec<u8>> {
    let start = pem.find("-----BEGIN CERTIFICATE-----")? + "-----BEGIN CERTIFICATE-----".len();
    let end = start + pem[start..].find("-----END CERTIFICATE-----")?;
    let b64: String = pem[start..end]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    data_encoding::BASE64.decode(b64.as_bytes()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_addresses_become_voice_links() {
        assert_eq!(server_url("sm").as_deref(), Some("https://sm:29203/voice/"));
        assert_eq!(
            server_url("192.168.1.5").as_deref(),
            Some("https://192.168.1.5:29203/voice/")
        );
        assert_eq!(
            server_url("sm.local:9000").as_deref(),
            Some("https://sm.local:9000/voice/")
        );
        assert_eq!(
            server_url("http://plant:8080").as_deref(),
            Some("http://plant:8080/voice/")
        );
        assert_eq!(
            server_url("https://voice.example.com/voice").as_deref(),
            Some("https://voice.example.com/voice/")
        );
        assert_eq!(
            server_url("[fe80::1]").as_deref(),
            Some("https://[fe80::1]:29203/voice/")
        );
        assert_eq!(server_url("ftp://x"), None);
        assert_eq!(server_url("a b"), None);
        assert_eq!(server_url("https://user@host/"), None);
        assert_eq!(server_url(""), None);
    }

    #[test]
    fn origins_compare_with_default_ports() {
        assert!(same_origin(
            "https://sm:29203/voice/ws",
            "https://sm:29203/voice/"
        ));
        assert!(same_origin(
            "https://Voice.example.com/x",
            "https://voice.example.com:443/voice/"
        ));
        assert!(!same_origin(
            "https://sm:7963/voice/",
            "https://sm:29203/voice/"
        ));
        assert!(!same_origin(
            "http://sm:29203/voice/",
            "https://sm:29203/voice/"
        ));
        assert!(!same_origin(
            "https://evil.example/sm:29203",
            "https://sm:29203/voice/"
        ));
        assert_eq!(host_port("https://sm/voice/").as_deref(), Some("sm:443"));
    }

    #[test]
    fn a_damaged_settings_file_is_set_aside_not_reset() {
        let dir = idn_common::testing::TempDir::new("voice-app-settings");
        let p = dir.join("voice-app.json");
        assert_eq!(Settings::load_from(&p), Settings::default());
        std::fs::write(&p, b"{ half a file").unwrap();
        assert_eq!(Settings::load_from(&p), Settings::default());
        assert!(!p.exists(), "set aside, so a save can't write over it");
        let aside = std::fs::read_dir(&*dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("voice-app.json.bad-")
            });
        assert!(aside);
    }

    #[test]
    fn pem_certificates_give_their_fingerprint() {
        let der = b"not really a certificate, but bytes".to_vec();
        let pem = format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            data_encoding::BASE64.encode(&der)
        );
        assert_eq!(pem_der(&pem), Some(der.clone()));
        let fp = fingerprint(&der);
        assert_eq!(fp.len(), 32 * 3 - 1);
        assert!(
            fp.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase() || c == ':')
        );
    }
}
