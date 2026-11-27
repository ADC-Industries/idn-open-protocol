//! A new site on this machine (idn-node.md §N.11.7): the IDN
//! Appliance's first start sets one up when none is found, or when its person chooses to
//! instead of joining one. The Site Manager's keys made in its folder here, this machine's
//! own certificate issued from them before it runs (no join, no code: the Node holds the
//! new site's keys itself), the Site Manager switched on with its site password, and this
//! machine's server set to it.

use super::*;
use idn_open_protocol::trust::{Authority, AuthorityConfig};

/// The server of a site this machine runs: its own Site Manager's discovery port.
pub(crate) const OWN_SERVER: &str = "127.0.0.1:5959";

impl NodeCore {
    /// Makes a new site here with `password` (its Site Manager's panel; 6 characters at
    /// least). Refused while this machine is in a site already.
    pub fn create_site(&self, password: &str) -> io::Result<()> {
        // One at a time (a double click, or the panel and the LAN panel at once: two CAs
        // made, the machine pinning one and its Site Manager using the other).
        static MAKING: Mutex<()> = Mutex::new(());
        let _one = MAKING.lock_ok();
        if password.chars().count() < 6 {
            return Err(idn_common::err::invalid_input(
                "the site password needs 6 characters at least",
            ));
        }
        let s = self.settings();
        if s.server.is_some() || s.sitemanager.on {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "this machine is in a site already (or runs one)",
            ));
        }
        // A site this machine ran before (its Site Manager off since): set aside, not
        // reused — its devices, users and denials would carry over into the "new" site,
        // and one that had denied this machine refused it for good (bug round 2026-11-09).
        let old = self.site_manager_dir();
        if old.join("ca.key").exists() {
            let secs = idn_common::epoch::secs();
            let mut name = old.file_name().unwrap_or_default().to_os_string();
            name.push(format!(".old-{secs}"));
            std::fs::rename(&old, old.with_file_name(&name))?;
            self.log(&format!(
                "site: the site this machine ran before set aside ({})",
                name.to_string_lossy()
            ));
        }
        let dir = self.make_site_manager_dir()?;
        // The site's keys, and this machine's certificate from them, while nothing else
        // has the folder open (the Site Manager starts below).
        let (ca, cert) = {
            let authority = Authority::open(AuthorityConfig {
                state_dir: dir,
                passphrase: None,
            })?;
            // A new site takes machines by code or approval (extensions §E.10.10).
            authority.set_join_mode(idn_open_protocol::trust::JoinMode::CodeOrApproval)?;
            let (cert, _) = authority.enrol_approved(&self.device_key.spki(), &s.name, "app")?;
            (authority.ca_der(), cert)
        };
        idn_open_protocol::trust::install_issued(&self.dir, &ca, &cert)?;
        self.log(&format!(
            "site: a new site made on this machine (its Site Manager on port {}), this machine in it",
            s.sitemanager.port.unwrap_or(29202)
        ));
        self.set(Change {
            sitemanager: vec![
                ("on".into(), "true".into()),
                ("password".into(), password.into()),
            ],
            server: Some(Some(OWN_SERVER.to_owned())),
            auto_site: Some(false),
            ..Default::default()
        })
    }
}

impl NodeCore {
    /// This machine's key (its SPKI, hex), for the Site Manager it runs.
    pub fn device_spki_hex(&self) -> String {
        idn_common::hex::hex(&self.device_key.spki())
    }

    /// After a restore of the Site Manager this machine runs (the site restores itself;
    /// F6-4): the certificate it issued to this machine from the
    /// restored keys, taken in, and the site joined again with it. Nothing when there is
    /// none.
    pub fn take_host_issued(&self, dir: &std::path::Path) {
        use idn_open_protocol::backup::{HOST_CA, HOST_CERT};
        let (ca_p, cert_p) = (dir.join(HOST_CA), dir.join(HOST_CERT));
        if !cert_p.exists() {
            return;
        }
        let r = std::fs::read(&ca_p).and_then(|ca| {
            let cert = std::fs::read(&cert_p)?;
            idn_open_protocol::trust::install_issued(&self.dir, &ca, &cert)
        });
        let _ = std::fs::remove_file(&cert_p);
        let _ = std::fs::remove_file(&ca_p);
        match r {
            Ok(()) => {
                self.log("site: restored from a backup: this machine's certificate from the restored site's keys");
                self.rebuild_member(None);
            }
            Err(e) => self.log(&format!(
                "site: restored from a backup, but this machine's certificate couldn't be taken in: {e}"
            )),
        }
    }
}
