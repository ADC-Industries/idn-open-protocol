//! Devices: the pairing window, approving and denying, relays, revocation, telling devices.

use super::*;

impl Authority {
    /// Opens the pairing window for `duration` ("Add device"). Requests that arrive while
    /// it is open are shown for approval; others are told to try later.
    /// At most a day: a larger value (from a UI or a remote admin) is cut to that rather
    /// than overflowing the clock (TR-2).
    pub fn open_pairing(&self, duration: Duration) {
        let until = Instant::now() + duration.min(MAX_PAIRING);
        self.0.state.lock_ok().pairing_until = Some(until);
    }

    /// Closes the pairing window; pending requests are cancelled.
    pub fn close_pairing(&self) {
        self.0.state.lock_ok().pairing_until = None;
    }

    /// Whether the pairing window is open.
    pub fn pairing_open(&self) -> bool {
        self.0
            .state
            .lock_ok()
            .pairing_until
            .is_some_and(|t| Instant::now() < t)
    }

    /// Requests waiting for a decision.
    pub fn pending(&self) -> Vec<JoinRequest> {
        self.0
            .state
            .lock_ok()
            .pending
            .values()
            .filter(|p| p.decision.is_none())
            .map(|p| p.req.clone())
            .collect()
    }

    /// Approves a pending request: the device gets its certificate at once.
    pub fn approve(&self, id: u64) -> io::Result<()> {
        self.decide(id, true, None)
    }

    /// A device name as the site keeps it ([`approve_as`](Self::approve_as)): control and
    /// bidi characters made spaces, at most 128 characters, trimmed. For naming its Node
    /// the same.
    pub fn clean_name(raw: &str) -> String {
        display_name(raw)
    }

    /// Approves a pending request under a name of the operator's (`None`, or empty: the
    /// one it asked with): its certificate carries that name, so the site's records,
    /// directory and the device itself ([`Enrollment::name_in_site`](crate::trust::Enrollment::name_in_site))
    /// all have it.
    pub fn approve_as(&self, id: u64, name: Option<&str>) -> io::Result<()> {
        let name = name.map(display_name).filter(|n| !n.is_empty());
        self.decide(id, true, name)
    }

    /// Denies a pending request. The device is refused again automatically until
    /// [`clear_denied`](Authority::clear_denied).
    pub fn deny(&self, id: u64) -> io::Result<()> {
        self.decide(id, false, None)
    }

    fn decide(&self, id: u64, ok: bool, name: Option<String>) -> io::Result<()> {
        let mut st = self.0.state.lock_ok();
        match st.pending.get_mut(&id) {
            Some(p) if p.decision.is_none() => {
                p.decision = Some(ok);
                p.name = name;
                Ok(())
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no pending request {id}"),
            )),
        }
    }

    /// Marks machine `fingerprint` as a pairing relay, or not. Only a
    /// relay gets the site's pairing codes to show (`idn_pairing_code`), so only a client
    /// pairing through one can be PIN-verified (`KeyRequest::code_ok`); a request through
    /// any other machine waits for a person's approval. Before, any machine of the site
    /// could ask for its own code and verify its own request: a compromised display box
    /// got a lasting API key with nobody approving. A relay must be an enrolled machine,
    /// not revoked; revoking a machine ends it being one.
    pub fn set_relay(&self, fingerprint: &str, on: bool) -> io::Result<()> {
        {
            let mut st = self.0.state.lock_ok();
            if on {
                let enrolled = st
                    .devices
                    .iter()
                    .any(|r| r.info.fingerprint == fingerprint && !r.revoked);
                if !enrolled {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "no such enrolled machine",
                    ));
                }
                st.relays.insert(fingerprint.to_owned());
            } else {
                st.relays.remove(fingerprint);
                st.pairing_codes.remove(fingerprint);
            }
        }
        self.save()
    }

    /// The machines marked as pairing relays ([`set_relay`](Self::set_relay)).
    pub fn relays(&self) -> Vec<String> {
        self.0.state.lock_ok().relays.iter().cloned().collect()
    }

    /// Whether machine `fingerprint` is a pairing relay.
    pub fn is_relay(&self, fingerprint: &str) -> bool {
        self.0.state.lock_ok().relays.contains(fingerprint)
    }

    /// Revokes a device by fingerprint: every device refuses it from now on (live sessions
    /// are closed), and its joins are refused until cleared.
    pub fn revoke(&self, fingerprint: &str) -> io::Result<()> {
        let dev = {
            let mut st = self.0.state.lock_ok();
            // Every active certificate of the device: it may hold more than one (it joined
            // again after losing its certificate), and the CRL must list the one in use.
            let mut newest = None;
            for r in st
                .devices
                .iter_mut()
                .filter(|r| r.info.fingerprint == fingerprint && !r.revoked)
            {
                r.revoked = true;
                newest = Some(r.clone());
            }
            let Some(r) = newest else {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "no such enrolled device",
                ));
            };
            // A relay no more.
            st.relays.remove(fingerprint);
            // Revoked devices may not simply join again (with the passphrase) (§E.10.6).
            st.denied.insert(fingerprint.to_owned());
            r
        };
        self.save()?;
        self.reissue_crl()?;
        self.tell_revoked(fingerprint);
        self.event(AuthorityEvent::Revoked(self.enrolled(&dev)));
        // The site's device count, to those watching the directory.
        self.directory_changed(Vec::new(), 0);
        Ok(())
    }

    /// The certificate `serial` just issued to `fingerprint` taken back: the device
    /// withdrew its join (§E.10.4). Revoked for the CRL; not listed, not refused.
    pub(in crate::trust) fn withdraw(&self, fingerprint: &str, serial: &[u8]) -> io::Result<()> {
        let name = {
            let mut st = self.0.state.lock_ok();
            let Some(r) = st
                .devices
                .iter_mut()
                .find(|r| r.info.fingerprint == fingerprint && r.info.serial == serial)
            else {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "no such certificate",
                ));
            };
            (r.revoked, r.withdrawn) = (true, true);
            let name = r.info.name.clone();
            st.relays.remove(fingerprint);
            st.pairing_codes.remove(fingerprint);
            name
        };
        self.save()?;
        self.reissue_crl()?;
        self.event(AuthorityEvent::Withdrawn {
            name,
            fingerprint: fingerprint.to_owned(),
        });
        Ok(())
    }

    /// Lets a denied or revoked device ask to join again.
    pub fn clear_denied(&self, fingerprint: &str) -> io::Result<()> {
        self.0.state.lock_ok().denied.remove(fingerprint);
        self.save()
    }

    /// Every device ever enrolled, once each, in its latest state: a device revoked and
    /// later accepted again shows as active. (Old certificates stay revoked.)
    pub fn devices(&self) -> Vec<EnrolledDevice> {
        let st = self.0.state.lock_ok();
        let mut latest: Vec<&Record> = Vec::new();
        for r in &st.devices {
            match latest
                .iter_mut()
                .find(|x| x.info.fingerprint == r.info.fingerprint)
            {
                Some(x) => *x = r, // later records are newer
                None => latest.push(r),
            }
        }
        latest
            .into_iter()
            .filter(|r| !r.withdrawn)
            .map(|r| enrolled(r, &st))
            .collect()
    }

    /// Issues a certificate to a device approved by other means than the trust
    /// connection's pairing (an enrolment token, spec/extensions/idn-industrial.md §I.5):
    /// recorded, revocable and reported as any other. Refused for a denied fingerprint.
    pub fn enrol_approved(
        &self,
        spki: &[u8],
        name: &str,
        kind: &str,
    ) -> io::Result<(Vec<u8>, EnrolledDevice)> {
        super::super::pki::public_from_spki(spki).ok_or_else(|| err("not an Ed25519 key"))?;
        let fp = super::super::pki::fingerprint(spki);
        let name = display_name(name);
        if self.is_denied(&fp) {
            self.event(AuthorityEvent::Refused {
                name,
                fingerprint: fp,
            });
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "this device was denied or revoked",
            ));
        }
        let kind = match kind {
            k @ ("output" | "input" | "app") => k,
            _ => "app",
        };
        let (cert, rec) = self.enrol(spki, &name, kind)?;
        let dev = self.enrolled(&rec);
        self.event(AuthorityEvent::Approved(dev.clone()));
        Ok((cert, dev))
    }

    /// Whether `fingerprint` is denied (its joins refused until cleared), revoked
    /// included.
    pub fn is_denied(&self, fingerprint: &str) -> bool {
        self.0.state.lock_ok().denied.contains(fingerprint)
    }

    pub(super) fn enrolled(&self, r: &Record) -> EnrolledDevice {
        enrolled(r, &self.0.state.lock_ok())
    }

    /// Pushes a directory change to every subscribed connection but `from` (§E.12.2),
    /// writing outside the state lock (see `reissue_crl`).
    pub(super) fn directory_changed(&self, children: Vec<Element>, from: u64) {
        let msg = {
            let mut m = Element::new("idn_directory_change").with_attr(
                "devices",
                &device_count(&self.0.state.lock_ok()).to_string(),
            );
            m.children = children;
            m.to_xml()
        };
        let to: Vec<_> = {
            let st = self.0.state.lock_ok();
            st.online
                .iter()
                .filter(|(_, id, _)| *id != from && st.directory_subs.contains(id))
                .map(|(_, _, w)| w.clone())
                .collect()
        };
        for w in to {
            let _ = proto::send(&mut *w.lock_ok(), &msg);
        }
    }

    fn tell_revoked(&self, fingerprint: &str) {
        // Taken off the list under the lock, told after releasing it (see reissue_crl).
        let (gone, unlisted): (Vec<_>, Vec<Element>) = {
            let mut st = self.0.state.lock_ok();
            let (gone, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut st.online)
                .into_iter()
                .partition(|(fp, _, _)| fp == fingerprint);
            st.online = keep;
            // Its endpoints, reports and directory watch go now, not when its connection
            // ends (a device that ignores the notice and stays connected stayed listed).
            let mut unlisted = Vec::new();
            for (_, id, _) in &gone {
                st.node_reports.remove(id);
                st.directory_subs.remove(id);
                if st.endpoints.remove(id).is_some() && unlisted.is_empty() {
                    unlisted.push(Element::new("gone").with_attr("fp", fingerprint));
                }
            }
            (gone, unlisted)
        };
        if !unlisted.is_empty() {
            self.directory_changed(unlisted, 0);
        }
        for (_, _, w) in gone {
            let mut w = w.lock_ok();
            let _ = proto::send(&mut *w, "<idn_revoked/>");
            // Close only our side of the TLS pipe: the pump sends the notice and a
            // close_notify first. Closing the socket outright lost the notice (lab X-t4).
            let _ = w.shutdown(std::net::Shutdown::Write);
        }
    }
}

fn enrolled(r: &Record, st: &State) -> EnrolledDevice {
    EnrolledDevice {
        fingerprint: r.info.fingerprint.clone(),
        name: r.info.name.clone(),
        kind: r.info.kind.clone(),
        admin: r.info.admin,
        revoked: r.revoked,
        online: st.online.iter().any(|(fp, _, _)| *fp == r.info.fingerprint),
        relay: st.relays.contains(&r.info.fingerprint),
    }
}
