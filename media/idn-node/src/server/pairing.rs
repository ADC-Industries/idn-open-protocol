//! Joining and pairing: tokens, the panel's PIN, approvals, signed updates.

use super::*;

impl NodeCore {
    /// Uses a panel token: once, within 60 s of its issue; whether it is full
    /// ([`NodeCore::issue_token`]).
    pub fn take_token(&self, t: &str) -> Option<bool> {
        let mut v = self.tokens.lock_ok();
        v.retain(|(_, at, _)| at.elapsed() < Duration::from_secs(60));
        let i = v.iter().position(|(x, _, _)| {
            idn_open_protocol::trust::proto::same(x.as_bytes(), t.as_bytes())
        })?;
        Some(v.remove(i).2)
    }

    /// An update offered by the Site Manager (§N.23): checked, fetched and installed (now
    /// or at the next start) on a thread of its own.
    pub(super) fn take_update(self: &Arc<Self>, e: &Element) -> io::Result<()> {
        use crate::update;
        let (m, p) = update::check_offer(e, self.update_managed_by().as_ref())?;
        // Windows, the Node at log-on: no administrator to run the installer.
        if cfg!(windows)
            && !self.service.load(Ordering::Relaxed)
            && !idn_open_protocol::release::is_host_product(&m.product)
        {
            return Err(io::Error::other(
                "update: this machine runs the IDN Node at log-on, not as a service: install the update there by hand",
            ));
        }
        let host = self
            .settings()
            .server
            .ok_or_else(|| io::Error::other("update: not in a site"))?;
        let host = host
            .rsplit_once(':')
            .map_or(host.as_str(), |x| x.0)
            .to_owned();
        let when_now = e.attr("when") != Some("restart");
        {
            // One at a time: a second offer while one downloads or installs wrote the same
            // file from two threads.
            // (The helper's files count too: one it is installing is in progress.)
            let busy = |p: &update::Progress| p.state == "downloading" || p.state == "installing";
            if self.update_progress().is_some_and(|p| busy(&p)) {
                return Err(io::Error::other("update: one is in progress already"));
            }
            let mut u = self.update.lock_ok();
            if u.as_ref().is_some_and(busy) {
                return Err(io::Error::other("update: one is in progress already"));
            }
            *u = Some(update::Progress {
                state: "downloading".into(),
                version: m.version.clone(),
                ..Default::default()
            });
        }
        self.log(&format!(
            "update {}: fetching it from the Site Manager",
            m.version
        ));
        let (core, e) = (self.clone(), e.clone());
        std::thread::Builder::new()
            .name("idn-node-update".into())
            .spawn(move || {
                let set = |state: &str, error: &str, progress: u8| {
                    *core.update.lock_ok() = Some(update::Progress {
                        state: state.into(),
                        version: m.version.clone(),
                        error: error.into(),
                        progress,
                    });
                };
                let got = update::fetch(&core.dir, &host, &e, &m, &p, &|pc| {
                    set("downloading", "", pc)
                });
                let r = got.and_then(|pkg| {
                    // Another product's: handed to the hosting program to install.
                    if idn_open_protocol::release::is_host_product(&m.product) {
                        set("installing", "", 100);
                        let r = match update::HOST_PRODUCTS.get() {
                            Some(f) => f(&m, &pkg),
                            None => Err(io::Error::other("no installer for it here")),
                        };
                        if r.is_ok() {
                            *core.update.lock_ok() = None;
                        }
                        return r;
                    }
                    if when_now {
                        set("installing", "", 100);
                        // Handed to the helper: from here its files say how it goes (a
                        // package it refused left "installing" here for good, every next
                        // offer refused as one in progress; bug run B).
                        update::install(
                            &core.dir,
                            &m.version,
                            &pkg,
                            core.service.load(Ordering::Relaxed),
                            &idn_open_protocol::build_line(),
                            &update::proof_of(&e),
                        )
                        .map(|()| {
                            *core.update.lock_ok() = None;
                        })
                    } else {
                        update::stage(&core.dir, &m.version, &pkg, &update::proof_of(&e)).map(
                            |()| {
                                *core.update.lock_ok() = None;
                            },
                        )
                    }
                });
                match r {
                    Ok(()) => core.log(&format!(
                        "update {}: {}",
                        m.version,
                        if when_now {
                            "installing"
                        } else {
                            "installed at the next start"
                        }
                    )),
                    Err(err) => {
                        core.log(&format!("update {}: {err}", m.version));
                        set("failed", &err.to_string(), 0);
                    }
                }
            })?;
        Ok(())
    }

    /// Checks a PIN that leaves simple mode (§N.20): none set, anything goes; five wrong
    /// in a row, a minute's wait.
    pub(super) fn check_panel_pin(&self, pin: &str) -> Result<(), String> {
        let Some(hash) = self.settings().panel_pin else {
            return Ok(());
        };
        // The try counted as wrong before the (slow) check: tries at the same time can't
        // all slip under the limit (as logins).
        self.pin_lock
            .check(|| idn_open_protocol::web::verify_password(&hash, pin.trim()))
    }

    /// Where the program at `exe` stands (§N.8.1).
    pub fn approval(&self, exe: &str) -> Approval {
        let s = self.settings();
        if !s.approve_programs {
            return Approval::Allowed;
        }
        if !exe.is_empty() {
            if let Some(r) = s.programs.iter().find(|r| super::same_path(&r.path, exe)) {
                return if r.allow {
                    Approval::Allowed
                } else {
                    Approval::Blocked
                };
            }
            if installed_with_node(exe) {
                return Approval::Allowed;
            }
        }
        Approval::Pending
    }
}
