//! Routes (§A) the Node applies and pushes to its receivers.

use super::*;

impl NodeCore {
    /// Route changes, from this machine or the Site Manager (`from_site`; idn-routing.md
    /// §A.3, §A.4): kept in `routes.xml`, then told to the programs offering the receivers.
    /// A machine locked by the site takes them from the Site Manager only (§N.31).
    pub fn apply_routes(&self, changes: &[RouteChange], from_site: bool) -> io::Result<()> {
        if !from_site && self.site_locked() {
            return Err(locked());
        }
        let changed: Vec<String> = {
            // On a copy, put in place once saved: a save that failed left the changes
            // in force while the answer said they failed (bug round F, F6-10).
            let mut held = self.routes.lock_ok();
            let mut t = held.clone();
            let mut changed: Vec<String> = changes
                .iter()
                .filter(|c| t.apply(c))
                .map(|c| c.receiver.clone())
                .collect();
            changed.sort();
            changed.dedup();
            if !changed.is_empty() {
                t.save(&self.dir.join("routes.xml"))?;
                *held = t;
            }
            changed
        };
        for c in changes {
            self.log(&match &c.to {
                Some((tx, tx_ch)) => format!(
                    "route: {} channel {} <- {tx} channel {}",
                    c.receiver,
                    c.ch + 1,
                    tx_ch + 1
                ),
                None => format!("route: {} channel {} cleared", c.receiver, c.ch + 1),
            });
        }
        for name in changed {
            self.push_routes(&name);
        }
        Ok(())
    }

    /// The receiver's `routes` as its program gets them: the routes, the latency the site
    /// set (§A.12) and its sync group's floor (§A.14).
    pub(super) fn routes_for(&self, name: &str) -> Element {
        let mut e = {
            let t = self.routes.lock_ok();
            routes_element(name, &t.routes(name), t.latency(name))
        };
        if let Some(f) = self.floors.lock_ok().get(name) {
            e = e.with_attr("floor_ms", &f.to_string());
        }
        e
    }

    /// A receiver's sync-group floor from the Site Manager (§A.14; `None` clears it):
    /// kept while the Node runs and told to the program with its routes.
    pub fn set_receiver_floor(&self, receiver: &str, ms: Option<u32>) {
        let changed = {
            let mut f = self.floors.lock_ok();
            match ms.filter(|&m| m > 0) {
                Some(m) => f.insert(receiver.to_owned(), m) != Some(m),
                None => f.remove(receiver).is_some(),
            }
        };
        if changed {
            self.push_routes(receiver);
        }
    }

    /// The receiver's routes (and latency), to the programs offering it.
    pub(super) fn push_routes(&self, name: &str) {
        let routes = self.routes_for(name);
        {
            let owners: Vec<u64> = self
                .clients
                .lock_ok()
                .iter()
                .filter(|(_, c)| {
                    c.regs
                        .values()
                        .any(|r| r.kind == "audio_rx" && r.name == name)
                })
                .map(|(id, _)| *id)
                .collect();
            for cid in owners {
                self.tell(cid, &routes);
            }
        }
    }

    /// A receiver's latency (idn-routing.md §A.12), from this machine or the Site Manager
    /// (`from_site`; `None` clears it: the program's own): kept in `routes.xml` and told to
    /// the program offering the receiver with its routes.
    pub fn set_receiver_latency(
        &self,
        receiver: &str,
        ms: Option<u32>,
        from_site: bool,
    ) -> io::Result<()> {
        if !from_site && self.site_locked() {
            return Err(locked());
        }
        if receiver.trim().is_empty() {
            return Err(idn_common::err::invalid_input("latency: no receiver"));
        }
        {
            let mut t = self.routes.lock_ok();
            if !t.set_latency(receiver, ms) {
                return Ok(());
            }
            t.save(&self.dir.join("routes.xml"))?;
        }
        self.log(&match ms {
            Some(ms) => format!("latency: {receiver} {ms} ms"),
            None => format!("latency: {receiver} as its program sets it"),
        });
        self.push_routes(receiver);
        Ok(())
    }

    /// Starts the site's file channel (media files, §N.25) once the machine is enrolled;
    /// it needs the site's certificate, which comes with the approval.
    pub(super) fn ensure_channel(&self) {
        let e = {
            let m = self.member.read_ok();
            if m.channel.is_some() {
                return;
            }
            match &m.enrollment {
                Some(e) if matches!(e.state(), TrustState::Enrolled { .. }) => e.clone(),
                _ => return,
            }
        };
        match idn_open_protocol::channel::Channel::start(
            idn_open_protocol::channel::ChannelConfig::new(e.clone()),
            self.media.clone(),
        ) {
            Ok(c) => {
                let mut m = self.member.write_ok();
                // Unless the membership changed meanwhile.
                if m.enrollment.as_ref().is_some_and(|x| Arc::ptr_eq(x, &e)) {
                    m.channel = Some(c);
                }
            }
            Err(x) => self.log(&format!("site: no file channel yet: {x}")),
        }
    }

    /// (Re)starts the membership from the settings: after a start, or a change of the
    /// name, server or passphrase. `admin_code` is the authority's one-time code to
    /// become an administrator.
    pub(super) fn rebuild_member(&self, admin_code: Option<String>) {
        let s = self.settings();
        let mut m = Member::default();
        let site = s.psk.as_ref().and_then(|k| match SiteKey::from_psk(k) {
            Ok(k) => Some(k),
            Err(e) => {
                self.log(&format!("passphrase: {e}"));
                None
            }
        });
        // The server set, else a site found on the network being asked (not kept yet).
        let server = s
            .server
            .clone()
            .or_else(|| self.trial_site().map(|t| t.server));
        if let Some(server) = server {
            // (No confirming here: there is no device-side "the code
            // matches" step; the Site Manager's approval is enough, §N.11.4.)
            match Enrollment::start_with_site_key(
                EnrollmentConfig {
                    discovery_server: server.clone(),
                    state_dir: self.dir.clone(),
                    name: s.name.clone(),
                    role: DeviceRole::App,
                    passphrase: None,
                    admin_code,
                },
                site.clone(),
            ) {
                Ok(e) => {
                    // Joined by code, the site is trusted only once its code is confirmed
                    // here (on before its thread starts, F6-8).
                    m.site = e.site_key();
                    m.enrollment = Some(e);
                }
                Err(e) => {
                    self.log(&format!("site: can't use the server {server}: {e}"));
                    m.error = Some(e.to_string());
                }
            }
        }
        if m.enrollment.is_none() {
            m.site = site;
        }
        *self.member.write_ok() = m;
    }
}
