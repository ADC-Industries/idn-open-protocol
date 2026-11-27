//! The site's channel directory (§E.12.2) and what the authority says with it: the
//! device count, the site's audio transmitters (§E.12.12).

use super::*;

impl Enrollment {
    /// Announces this device's channel endpoint to the authority (§E.12.2), now if
    /// connected and again on every reconnection.
    #[doc(hidden)]
    pub fn announce_endpoint(&self, port: u16) {
        // (Port 0: none any more, withdrawn; bug run B.)
        *self.endpoint.lock_ok() = Some(port).filter(|&p| p != 0);
        if let Some(h) = &self.node {
            h.send(Element::new("endpoint").with_attr("port", &port.to_string()));
            return;
        }
        self.trust_send(&format!(r#"<idn_endpoint port="{port}"/>"#));
    }

    /// Takes back endpoint `port` if it is still the one announced (a channel dropped:
    /// the directory listed its dead port for good, every request to the machine waiting
    /// out a connect to it; bug run B).
    pub(crate) fn withdraw_endpoint(&self, port: u16) {
        if *self.endpoint.lock_ok() == Some(port) {
            self.announce_endpoint(0);
        }
    }

    /// Asks the authority for the directory and its changes from now on (§E.12.2).
    #[doc(hidden)]
    pub fn watch_directory(&self) {
        let first = {
            let mut d = self.directory.lock_ok();
            let first = d.is_none();
            d.get_or_insert_with(BTreeMap::new);
            first
        };
        if first {
            match &self.node {
                Some(h) => h.send(Element::new("directory")),
                None => {
                    self.trust_send("<idn_directory/>");
                }
            }
        }
    }

    /// The channel endpoints known from the directory (empty until it arrives).
    #[doc(hidden)]
    pub fn directory(&self) -> Vec<DirEntry> {
        self.directory
            .lock_ok()
            .as_ref()
            .map(|d| d.values().cloned().collect())
            .unwrap_or_default()
    }

    /// One directory entry, by fingerprint.
    pub(crate) fn directory_entry(&self, fingerprint: &str) -> Option<DirEntry> {
        self.directory.lock_ok().as_ref()?.get(fingerprint).cloned()
    }

    /// The site's device count (enrolled, not revoked), once the directory is watched
    /// ([`Enrollment::watch_directory`]) and its authority said it.
    pub fn site_devices(&self) -> Option<usize> {
        *self.site_devices.lock_ok()
    }

    /// The site's audio transmitters `(name, channels, machine)` (§E.12.12), as its
    /// authority last said; asked for again when older than 5 s (the answer arrives a
    /// moment later). Empty until then, or outside a site.
    pub fn site_transmitters(&self) -> Vec<SiteTx> {
        let mut g = self.site_txs.lock_ok();
        if g.1.is_none_or(|t| t.elapsed() >= Duration::from_secs(5)) {
            g.1 = Some(Instant::now());
            self.trust_send("<idn_site_transmitters/>");
        }
        g.0.clone()
    }

    pub(super) fn apply_directory(&self, m: &Element, full: bool) {
        if let Some(n) = m.attr("devices").and_then(|v| v.parse().ok()) {
            *self.site_devices.lock_ok() = Some(n);
        }
        let mut d = self.directory.lock_ok();
        let d = d.get_or_insert_with(BTreeMap::new);
        if full {
            d.clear();
        }
        for c in &m.children {
            match c.name.as_str() {
                "device" => {
                    if let Some(e) = DirEntry::parse(c) {
                        d.insert(e.fingerprint.clone(), e);
                    }
                }
                "gone" => {
                    if let Some(fp) = c.attr("fp") {
                        d.remove(fp);
                    }
                }
                _ => {}
            }
        }
    }
}
