//! The site's options document (§E.20): set, removed, saved and pushed to every
//! connected device.

use super::*;

impl Authority {
    /// The site's options document (§E.20).
    pub fn options(&self) -> crate::trust::options::SiteOptions {
        self.0.state.lock_ok().options.clone()
    }

    /// Sets an option (site-wide, or for a device, output or input), saves the document
    /// and pushes it to every connected device. Returns the new version.
    pub fn set_option(
        &self,
        scope: &crate::trust::options::OptionScope,
        key: &str,
        value: &str,
    ) -> io::Result<u64> {
        self.change_options(scope, |o, name| o.set(scope, key, value, name))
    }

    /// Removes an option; see [`set_option`](Authority::set_option).
    pub fn unset_option(
        &self,
        scope: &crate::trust::options::OptionScope,
        key: &str,
    ) -> io::Result<u64> {
        self.change_options(scope, |o, _| Ok(o.unset(scope, key)))
    }

    pub(super) fn change_options(
        &self,
        scope: &crate::trust::options::OptionScope,
        f: impl FnOnce(&mut crate::trust::options::SiteOptions, &str) -> io::Result<u64>,
    ) -> io::Result<u64> {
        let (version, xml, online) = {
            let mut st = self.0.state.lock_ok();
            // The device's name beside a device scope, for the operator.
            let name = match scope {
                crate::trust::options::OptionScope::Device(fp) => st
                    .devices
                    .iter()
                    .find(|r| r.info.fingerprint == *fp)
                    .map(|r| display_name(&r.info.name))
                    .unwrap_or_default(),
                _ => String::new(),
            };
            let mut o = st.options.clone();
            let version = f(&mut o, &name)?;
            // (Whole or not at all: a cut-off file stopped the authority's next start.)
            super::super::pki::write_private(
                &self.0.dir.join("options.xml"),
                o.to_xml().as_bytes(),
            )?;
            st.options = o;
            (version, st.options.to_xml(), st.online.clone())
        };
        // Pushed after releasing the lock, like the CRL.
        for (_, _, w) in &online {
            let _ = proto::send(&mut *w.lock_ok(), &xml);
        }
        Ok(version)
    }
}
