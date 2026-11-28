//! The authority's trust connections (§E.10.2): devices joining and enrolling, the
//! first administrator (§E.10.8), enrolled devices' sessions and their admin requests.

use super::lan_certs::LAN_EVERY;
use super::*;

impl Authority {
    /// Serves one trust connection whose `<idn_trust>` hello has been read (§E.10.2)
    /// (tests).
    #[cfg(test)]
    pub(crate) fn serve(&self, s: TcpStream) {
        self.serve_hello(s, r#"<idn_trust version="2"/>"#)
    }

    /// [`serve`](Self::serve) for the hello `first` (its text): the device is told the
    /// site's join mode, and may join with a typed code (§E.10.10). A hello without
    /// version 2 or later is not served (§E.10.2).
    pub(crate) fn serve_hello(&self, mut s: TcpStream, first: &str) {
        // Blocking, whatever the listener that accepted it was (an accepted socket may
        // inherit non-blocking mode): the reads below wait on their timeouts, not spin.
        let _ = s.set_nonblocking(false);
        let version = crate::xml::parse(first.trim_end_matches('\0'))
            .and_then(|e| e.attr("version").and_then(|v| v.parse::<u32>().ok()));
        let peer = s.peer_addr().ok().map(|a| a.ip());
        if !version.is_some_and(|v| v >= 2) {
            crate::log::log(&format!(
                "trust: {peer:?} said hello without version 2: not served"
            ));
            return;
        }
        // A device that stops reading must not block the authority's writes (and the
        // TLS pump of an enrolled session, which writes to a clone of this socket).
        let _ = s.set_write_timeout(Some(crate::transport::WRITE_TIMEOUT));
        let hello = Element::new("idn_trust_ok")
            .with_attr("version", "2")
            .with_attr("ca", &b64(&self.0.ca.cert_der()))
            .with_attr("join", self.join_mode().as_str())
            .to_xml();
        if proto::send(&mut s, &hello).is_err() {
            return;
        }
        let by = std::time::Instant::now() + Duration::from_secs(30);
        let Ok(Some(m)) = proto::recv_by(&mut s, proto::MAX_PLAIN, by) else {
            return;
        };
        let _ = s.set_read_timeout(Some(Duration::from_secs(30)));
        match m.name.as_str() {
            "idn_join" => {
                if let Err(e) = self.join(&mut s, &m, peer) {
                    crate::log::log(&format!("trust: join from {peer:?} failed: {e}"));
                }
            }
            "idn_starttls" => {
                if let Err(e) = self.enrolled_session(s) {
                    crate::log::log(&format!("trust: session from {peer:?} ended: {e}"));
                }
            }
            _ => {}
        }
    }

    pub(super) fn join(
        &self,
        s: &mut TcpStream,
        m: &Element,
        peer: Option<IpAddr>,
    ) -> io::Result<()> {
        let status = |s: &mut TcpStream, state: &str| {
            proto::send(s, &format!(r#"<idn_join_status state="{state}"/>"#))
        };
        let spki = m
            .attr("key")
            .and_then(unb64)
            .ok_or_else(|| err("idn_join without a key"))?;
        crate::trust::pki::public_from_spki(&spki).ok_or_else(|| err("not an Ed25519 key"))?;
        let fp = crate::trust::pki::fingerprint(&spki);
        let name = display_name(m.attr("name").unwrap_or(""));
        let kind = match m.attr("kind") {
            Some(k @ ("output" | "input" | "app")) => k.to_owned(),
            _ => "app".into(),
        };
        if self.0.state.lock_ok().denied.contains(&fp) {
            self.event(AuthorityEvent::Refused {
                name: name.clone(),
                fingerprint: fp.clone(),
            });
            return status(s, "denied");
        }
        let ca_spki = self.0.ca.key().spki();
        let mode = self.join_mode();
        // Hybrid: a valid passphrase proof approves at once (§E.10.4): `psk_mac2`, which
        // covers the name, kind and time, taken once within its window; without one the
        // device waits for approval. Not in `code_and_approval`, where
        // every machine types a code and is approved (§E.10.10).
        let now_s = idn_common::epoch::secs();
        let proof = m
            .attr("psk_mac2")
            .and_then(unb64)
            .zip(m.attr("psk_at").and_then(|a| a.parse::<u64>().ok()))
            .filter(|(_, at)| at.abs_diff(now_s) <= PROOF_WINDOW);
        let site = self
            .0
            .site
            .as_ref()
            .filter(|_| mode != crate::trust::JoinMode::CodeAndApproval);
        if let (Some(site), Some((mac, at))) = (site, proof) {
            let good = proto::same(
                &mac,
                &site.join_mac2(
                    &spki,
                    &ca_spki,
                    m.attr("name").unwrap_or(""),
                    m.attr("kind").unwrap_or(""),
                    at,
                ),
            );
            let fresh = good && {
                let mut st = self.0.state.lock_ok();
                st.proofs_seen
                    .retain(|_, t| t.abs_diff(now_s) <= 2 * PROOF_WINDOW);
                let key: [u8; 32] = mac.as_slice().try_into().unwrap_or([0; 32]);
                st.proofs_seen.insert(key, now_s).is_none()
            };
            if fresh {
                let theirs = b64(&site.join_mac("authority", &spki, &ca_spki));
                let (cert, rec) = self.enrol(&spki, &name, &kind)?;
                proto::send(
                    s,
                    &format!(r#"<idn_join_status state="approved" psk_mac="{theirs}"/>"#),
                )?;
                proto::send(s, &format!(r#"<idn_cert cert="{}"/>"#, b64(&cert)))?;
                self.event(AuthorityEvent::Approved(self.enrolled(&rec)));
                return Ok(());
            }
            crate::log::log(&format!(
                "trust: {name}: passphrase proof did not match; approval needed"
            ));
        }
        // A typed code (§E.10.10), where the site takes codes.
        let typed = m
            .attr("pake")
            .and_then(unb64)
            .zip(m.attr("nd").and_then(unb64).filter(|n| n.len() == 32));
        if let Some((x, nd)) = typed.filter(|_| mode.takes_codes()) {
            let who = CodeJoiner {
                spki: &spki,
                ca_spki: &ca_spki,
                name_sent: m.attr("name").unwrap_or(""),
                kind_sent: m.attr("kind").unwrap_or(""),
                name: &name,
                kind: &kind,
                fp: &fp,
                peer,
            };
            return self.code_join(s, &who, &x, &nd, mode);
        }
        if mode == crate::trust::JoinMode::CodeAndApproval {
            return status(s, "code_needed");
        }
        if !self.pairing_open() {
            return status(s, "closed");
        }
        // Short authentication string with a commitment (§E.10.4).
        let na = crate::random::secure_random::<32>();
        proto::send(
            s,
            &format!(r#"<idn_commit c="{}"/>"#, b64(&proto::commit(&na))),
        )?;
        let by = std::time::Instant::now() + Duration::from_secs(30);
        let nonce = proto::recv_by(s, proto::MAX_PLAIN, by)?
            .filter(|e| e.name == "idn_nonce")
            .ok_or_else(|| err("expected idn_nonce"))?;
        s.set_read_timeout(Some(Duration::from_secs(30)))?;
        let nd = nonce
            .attr("n")
            .and_then(unb64)
            .filter(|n| n.len() == 32)
            .ok_or_else(|| err("bad nonce"))?;
        proto::send(s, &format!(r#"<idn_reveal n="{}"/>"#, b64(&na)))?;
        let code = proto::code(&spki, &ca_spki, &na, &nd);
        let (id, req) = {
            let mut st = self.0.state.lock_ok();
            let id = st.next_id;
            st.next_id += 1;
            let req = JoinRequest {
                id,
                name: name.clone(),
                kind: kind.clone(),
                fingerprint: fp.clone(),
                code,
                address: peer,
                by_code: false,
                asked_at: idn_common::epoch::secs(),
            };
            st.pending.insert(
                id,
                PendingJoin {
                    req: req.clone(),
                    decision: None,
                    name: None,
                },
            );
            (id, req)
        };
        // The event after `state` is let go, as everywhere else: `node_set` holds the
        // events lock while it looks at `state` (a deadlock otherwise).
        self.event(AuthorityEvent::JoinRequested(req));
        status(s, "pending")?;
        // Wait for the decision, the window closing, or the device leaving.
        let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
        let outcome = loop {
            let d = self
                .0
                .state
                .lock_ok()
                .pending
                .get(&id)
                .and_then(|p| p.decision);
            if let Some(d) = d {
                break Some(d);
            }
            if !self.pairing_open() {
                break None;
            }
            let mut b = [0u8; 1];
            match s.read(&mut b) {
                Ok(0) => break None,
                Err(e) if crate::transport::is_poll_timeout(&e) => {}
                _ => break None,
            }
        };
        let given = self
            .0
            .state
            .lock_ok()
            .pending
            .remove(&id)
            .and_then(|p| p.name);
        match outcome {
            Some(true) => {
                let (cert, rec) = self.enrol(&spki, given.as_deref().unwrap_or(&name), &kind)?;
                status(s, "approved")?;
                proto::send(s, &format!(r#"<idn_cert cert="{}"/>"#, b64(&cert)))?;
                self.event(AuthorityEvent::Approved(self.enrolled(&rec)));
                // Joined by code: a device with a person at it confirms the code there too,
                // for up to 10 minutes, and withdraws if it doesn't match (§E.10.4): its
                // certificate is taken back. A device that confirms, or one that doesn't
                // ask its person, just closes.
                let by = std::time::Instant::now() + Duration::from_secs(11 * 60);
                let _ = s.set_read_timeout(Some(Duration::from_secs(30)));
                loop {
                    match proto::recv_by(s, proto::MAX_PLAIN, by) {
                        Ok(Some(m)) if m.name == "idn_join_withdraw" => {
                            crate::log::log(&format!(
                                "trust: {name:?} withdrew its join (its code wasn't confirmed there): certificate taken back"
                            ));
                            self.withdraw(&fp, &rec.info.serial)?;
                            break;
                        }
                        Ok(Some(_)) => {}
                        // (Until the 11 minutes are up: past them every read times out at
                        // once, and on Windows a timeout is a poll timeout, so this went
                        // round for good; bug run 2026-11-25.)
                        Err(e)
                            if crate::transport::is_poll_timeout(&e)
                                && std::time::Instant::now() < by => {}
                        _ => break,
                    }
                }
            }
            Some(false) => {
                self.0.state.lock_ok().denied.insert(fp.clone());
                self.save()?;
                status(s, "denied")?;
                self.event(AuthorityEvent::Denied {
                    name,
                    fingerprint: fp,
                });
            }
            None => {
                let _ = status(s, "closed");
                self.event(AuthorityEvent::JoinCancelled(id));
            }
        }
        Ok(())
    }

    /// A join with a typed code (§E.10.10): one SPAKE2 run per waiting code, the
    /// authority's confirmation first, then the device's. A try of every code is taken
    /// before anything is sent; a wrong or missing confirmation keeps them taken.
    pub(super) fn code_join(
        &self,
        s: &mut TcpStream,
        w: &CodeJoiner,
        x: &[u8],
        nd: &[u8],
        mode: crate::trust::JoinMode,
    ) -> io::Result<()> {
        use crate::trust::join_code;
        let state = |st: &str| Element::new("idn_join_status").with_attr("state", st);
        if let Some(secs) = self.code_limited(w.peer) {
            crate::log::log(&format!(
                "trust: {}: a join code from {:?}, refused (too many wrong codes from there)",
                w.name, w.peer
            ));
            return proto::send(
                s,
                &state("code_limited")
                    .with_attr("retry", &secs.to_string())
                    .to_xml(),
            );
        }
        let codes = self.take_code_tries()?;
        if codes.is_empty() {
            return proto::send(s, &state("code_expired").to_xml());
        }
        let ids: Vec<u64> = codes.iter().map(|(id, _)| *id).collect();
        let na = crate::random::secure_random::<32>();
        let runs: Option<Vec<_>> = codes
            .iter()
            .map(|(id, code)| {
                join_code::authority_run(
                    code,
                    w.spki,
                    w.ca_spki,
                    w.name_sent,
                    w.kind_sent,
                    nd,
                    &na,
                    x,
                )
                .map(|r| (*id, r))
            })
            .collect();
        let wrong = |s: &mut TcpStream| {
            let (left, burned) = self.code_failed(&ids, w.peer);
            crate::log::log(&format!(
                "trust: {}: wrong join code from {:?} ({left} tries left)",
                w.name, w.peer
            ));
            self.event(AuthorityEvent::CodeWrong {
                name: w.name.to_owned(),
                address: w.peer,
                left,
            });
            self.burned(burned.into_iter());
            proto::send(
                s,
                &state("wrong_code")
                    .with_attr("left", &left.to_string())
                    .to_xml(),
            )
        };
        // A malformed message from the device: a failed try as any other.
        let Some(runs) = runs else {
            return wrong(s);
        };
        let mut challenge = Element::new("idn_code_challenge").with_attr("na", &b64(&na));
        for (_, (y, a, _)) in &runs {
            challenge.children.push(
                Element::new("run")
                    .with_attr("y", &b64(y))
                    .with_attr("confirm", &b64(a)),
            );
        }
        if let Err(e) = proto::send(s, &challenge.to_xml()) {
            let (_, burned) = self.code_failed(&ids, w.peer);
            self.burned(burned.into_iter());
            return Err(e);
        }
        let by = Instant::now() + Duration::from_secs(30);
        let confirm = match proto::recv_by(s, proto::MAX_PLAIN, by) {
            Ok(Some(e)) if e.name == "idn_code_confirm" => e.attr("confirm").and_then(unb64),
            _ => None,
        };
        let used = confirm.and_then(|c| {
            // Every run compared, so the time doesn't tell which.
            runs.iter()
                .filter(|(_, (_, _, d))| proto::same(d, &c))
                .map(|(id, _)| *id)
                .fold(None, |f: Option<u64>, id| f.or(Some(id)))
        });
        let Some(used) = used else {
            return wrong(s);
        };
        let others: Vec<u64> = ids.iter().copied().filter(|i| *i != used).collect();
        self.code_matched(used, &others)?;
        crate::log::log(&format!(
            "trust: {} ({}) typed join code {used}",
            w.name, w.fp
        ));
        self.event(AuthorityEvent::CodeUsed {
            id: used,
            name: w.name.to_owned(),
            fingerprint: w.fp.to_owned(),
        });
        if mode != crate::trust::JoinMode::CodeAndApproval {
            let (cert, rec) = self.enrol(w.spki, w.name, w.kind)?;
            proto::send(s, &state("approved").with_attr("by", "code").to_xml())?;
            proto::send(s, &format!(r#"<idn_cert cert="{}"/>"#, b64(&cert)))?;
            self.event(AuthorityEvent::Approved(self.enrolled(&rec)));
            return Ok(());
        }
        // Code and approval: waits for an operator, for as long as a code lasts, the
        // pairing window not needed (issuing the code opened the way).
        let (id, req) = {
            let mut st = self.0.state.lock_ok();
            let id = st.next_id;
            st.next_id += 1;
            let req = JoinRequest {
                id,
                name: w.name.to_owned(),
                kind: w.kind.to_owned(),
                fingerprint: w.fp.to_owned(),
                code: String::new(),
                address: w.peer,
                by_code: true,
                asked_at: idn_common::epoch::secs(),
            };
            st.pending.insert(
                id,
                PendingJoin {
                    req: req.clone(),
                    decision: None,
                    name: None,
                },
            );
            (id, req)
        };
        self.event(AuthorityEvent::JoinRequested(req));
        proto::send(s, &state("pending").with_attr("by", "code").to_xml())?;
        let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
        let end = Instant::now() + Duration::from_secs(crate::trust::join_code::CODE_LIFE_S);
        let outcome = loop {
            let d = self
                .0
                .state
                .lock_ok()
                .pending
                .get(&id)
                .and_then(|p| p.decision);
            if let Some(d) = d {
                break Some(d);
            }
            if Instant::now() >= end {
                break None;
            }
            let mut b = [0u8; 1];
            match s.read(&mut b) {
                Ok(0) => break None,
                Err(e) if crate::transport::is_poll_timeout(&e) => {}
                _ => break None,
            }
        };
        let given = self
            .0
            .state
            .lock_ok()
            .pending
            .remove(&id)
            .and_then(|p| p.name);
        match outcome {
            Some(true) => {
                let (cert, rec) = self.enrol(w.spki, given.as_deref().unwrap_or(w.name), w.kind)?;
                proto::send(s, &state("approved").with_attr("by", "code").to_xml())?;
                proto::send(s, &format!(r#"<idn_cert cert="{}"/>"#, b64(&cert)))?;
                self.event(AuthorityEvent::Approved(self.enrolled(&rec)));
            }
            Some(false) => {
                self.0.state.lock_ok().denied.insert(w.fp.to_owned());
                self.save()?;
                proto::send(s, &state("denied").to_xml())?;
                self.event(AuthorityEvent::Denied {
                    name: w.name.to_owned(),
                    fingerprint: w.fp.to_owned(),
                });
            }
            None => {
                let _ = proto::send(s, &state("closed").to_xml());
                self.event(AuthorityEvent::JoinCancelled(id));
            }
        }
        Ok(())
    }

    pub(super) fn enrol(
        &self,
        spki: &[u8],
        name: &str,
        kind: &str,
    ) -> io::Result<(Vec<u8>, Record)> {
        let (cert, info) = self.0.ca.issue(spki, name, kind, false)?;
        let rec = Record {
            info,
            revoked: false,
            withdrawn: false,
        };
        self.0.state.lock_ok().devices.push(rec.clone());
        self.save()?;
        // The site's device count, to those watching the directory.
        self.directory_changed(Vec::new(), 0);
        Ok((cert, rec))
    }

    /// The first administrator (§E.10.8): an enrolled device gives the one-time admin
    /// code inside its TLS trust connection and gets a certificate with the admin role
    /// in place of `info`'s. The code was once in the plain-text join, where whoever saw
    /// it and knew the passphrase could use it first.
    pub(super) fn claim_admin(
        &self,
        cert: &[u8],
        info: &CertInfo,
        code: &str,
    ) -> io::Result<(Vec<u8>, CertInfo)> {
        // Taken under the lock (one-time), given back if the certificate can't be made.
        let taken = {
            let mut st = self.0.state.lock_ok();
            let good = st
                .admin_code
                .as_deref()
                .is_some_and(|a| proto::same(a.as_bytes(), code.trim().as_bytes()));
            if !good {
                // A few wrong ones use it up.
                if st.admin_code.is_some() {
                    st.admin_tries += 1;
                    if st.admin_tries >= super::ADMIN_CODE_TRIES {
                        st.admin_code = None;
                        drop(st);
                        crate::log::log(&format!(
                            "site: the admin code is used up ({} wrong tries, the last from {})",
                            super::ADMIN_CODE_TRIES,
                            info.name
                        ));
                    }
                }
                // (PermissionDenied: a wrong code, not to be tried again; anything else
                // is told as worth a retry.)
                return Err(io::Error::other(WrongAdminCode));
            }
            st.admin_code.take()
        };
        let issued = x509_parser::parse_x509_certificate(cert)
            .map_err(err)
            .and_then(|(_, c)| {
                self.0
                    .ca
                    .issue(c.public_key().raw, &info.name, &info.kind, true)
            });
        let (der, admin) = match issued {
            Ok(i) => i,
            Err(e) => {
                self.0.state.lock_ok().admin_code = taken;
                return Err(e);
            }
        };
        // A record of its own beside the first certificate's (bug run 2026-11-24: that
        // one was overwritten, so revoking the machine never listed the first certificate,
        // which still worked elsewhere). The newer record is the machine's in the lists.
        self.0.state.lock_ok().devices.push(Record {
            info: admin.clone(),
            revoked: false,
            withdrawn: false,
        });
        // Not saved: taken back, the code with it (it was used up, the record only in
        // memory, no certificate delivered; bug run 2026-11-25).
        if let Err(e) = self.save() {
            let mut st = self.0.state.lock_ok();
            st.devices.retain(|r| r.info.serial != admin.serial);
            st.admin_code = taken;
            return Err(e);
        }
        Ok((der, admin))
    }

    /// An enrolled device's TLS trust connection: CRL pushes, revocation notice, and admin
    /// requests from admin devices (§E.10.2, §E.10.6, §E.10.8).
    pub(super) fn enrolled_session(&self, mut s: TcpStream) -> io::Result<()> {
        proto::send(&mut s, "<idn_starttls_ok/>")?;
        let _ = s.set_read_timeout(None);
        // No read timeout (an idle device is fine), so keepalive: a machine that vanished
        // without closing (power, cable, sleep) counts as offline within ~13 s, not never
        // (lab 395-B: no "offline" for a machine gone).
        crate::transport::keepalive(&s, std::time::Duration::from_secs(10));
        let raw = s.try_clone()?;
        let conn = rustls::ServerConnection::new(Arc::new(
            self.0.server.server_config_with(proto::TRUST_ALPN, false)?,
        ))
        .map_err(err)?;
        let (mut tls, peer_cert) =
            crate::secure::wrap_with_peer(s, rustls::Connection::Server(conn))?;
        let cert = peer_cert.ok_or_else(|| err("no client certificate"))?;
        let mut info = self.0.anchor.verify_with(&cert, false)?;
        // Revoked: marked so, on the deny list, or in the CRL. A certificate our CA signed
        // that is none of these but missing from devices.tsv (lost, damaged or restored
        // from an older backup) is a device we enrolled: learn it again rather than
        // telling it to wipe its enrolment.
        let in_crl = self.0.anchor.verify_with(&cert, true).is_err();
        let revoked = {
            let mut st = self.0.state.lock_ok();
            let known = st.devices.iter().find(|r| r.info.serial == info.serial);
            let revoked =
                in_crl || st.denied.contains(&info.fingerprint) || known.is_some_and(|r| r.revoked);
            if known.is_none() && !revoked {
                crate::log::log(&format!(
                    "trust: {:?} ({}) was missing from devices.tsv; added again from its certificate",
                    info.name, info.fingerprint
                ));
                st.devices.push(Record {
                    info: CertInfo {
                        name: display_name(&info.name),
                        ..info.clone()
                    },
                    revoked: false,
                    withdrawn: false,
                });
                drop(st);
                self.save()?;
            }
            revoked
        };
        if revoked {
            // Inside TLS, so the notice is authentic; flushed before closing (lab X-t4).
            let _ = proto::send(&mut tls, "<idn_revoked/>");
            let _ = tls.shutdown(std::net::Shutdown::Write);
            let mut b = [0u8; 64];
            let _ = tls.set_read_timeout(Some(Duration::from_secs(2)));
            while matches!(tls.read(&mut b), Ok(n) if n > 0) {}
            return Ok(());
        }
        tls.set_write_timeout(Some(crate::transport::WRITE_TIMEOUT))?;
        let w = Arc::new(Mutex::new(tls.try_clone()?));
        // Registered first, then sent the current CRL outside the state lock. A newer
        // CRL pushed in between may arrive first; devices ignore the older number.
        let (conn_id, crl) = {
            let mut st = self.0.state.lock_ok();
            let id = st.next_conn;
            st.next_conn += 1;
            st.online.push((info.fingerprint.clone(), id, w.clone()));
            (id, format!(r#"<idn_crl crl="{}"/>"#, b64(&st.crl_der)))
        };
        let address = raw.peer_addr().map(|a| a.ip()).ok();
        let r = (|| -> io::Result<()> {
            proto::send(&mut *w.lock_ok(), &crl)?;
            // The site's options, if any (§E.20.4).
            let options = {
                let st = self.0.state.lock_ok();
                (st.options.version > 0).then(|| st.options.to_xml())
            };
            if let Some(o) = options {
                proto::send(&mut *w.lock_ok(), &o)?;
            }
            // Inside TLS nothing follows a message that must stay unread: buffered reads.
            let mut rd = io::BufReader::new(tls.try_clone()?);
            let mut last_lan: Option<Instant> = None;
            while let Some(m) = proto::recv(&mut rd)? {
                // Revoked meanwhile (taken off `online`, told): nothing more it says is
                // served, an administrator's request least of all; the session ends.
                if !self
                    .0
                    .state
                    .lock_ok()
                    .online
                    .iter()
                    .any(|(_, id, _)| *id == conn_id)
                {
                    break;
                }
                match m.name.as_str() {
                    "idn_admin_claim" if !info.admin => {
                        let reply =
                            match self.claim_admin(&cert, &info, m.attr("code").unwrap_or("")) {
                                Ok((der, admin)) => {
                                    // (This session is the new certificate's from now on:
                                    // its leaving names that one.)
                                    info = admin;
                                    format!(r#"<idn_cert cert="{}"/>"#, b64(&der))
                                }
                                Err(e) => {
                                    let mut r = Element::new("idn_admin_claim_result")
                                        .with_attr("ok", "false")
                                        .with_attr("error", &e.to_string());
                                    // (Not the code's fault: the device may try again.)
                                    if !e.get_ref().is_some_and(|i| i.is::<WrongAdminCode>()) {
                                        r = r.with_attr("retry", "true");
                                    }
                                    r.to_xml()
                                }
                            };
                        proto::send(&mut *w.lock_ok(), &reply)?;
                    }
                    "idn_admin" if info.admin => {
                        let mut reply = self.admin(&m);
                        // (Its request's number echoed: two at once were answered in
                        // order and each caller took the first answer; bug run 2026-11-25.)
                        if let Some(r) = m.attr("req").filter(|r| r.len() <= 20) {
                            reply = reply.with_attr("req", r);
                        }
                        proto::send(&mut *w.lock_ok(), &reply.to_xml())?;
                    }
                    // Not an administrator: told so, rather than left waiting.
                    "idn_admin" => {
                        let mut reply = Element::new("idn_admin_result")
                            .with_attr("op", m.attr("op").unwrap_or(""))
                            .with_attr("ok", "false")
                            .with_attr("error", "not an administrator");
                        if let Some(r) = m.attr("req").filter(|r| r.len() <= 20) {
                            reply = reply.with_attr("req", r);
                        }
                        proto::send(&mut *w.lock_ok(), &reply.to_xml())?;
                    }
                    // The site's audio transmitters and their channel counts, from the
                    // machines' reports (§E.12.12): what a desk's routing view shows of
                    // the network's sources.
                    // A machine asks for its newest release (idn-node.md §N.42).
                    "idn_update_check" => {
                        let short = |k: &str| {
                            m.attr(k)
                                .unwrap_or("")
                                .chars()
                                .take(120)
                                .collect::<String>()
                        };
                        self.event(AuthorityEvent::UpdateCheck {
                            fingerprint: info.fingerprint.clone(),
                            product: short("product"),
                            version: short("version"),
                        });
                    }
                    // A client's pairing relayed by this machine (idn-node.md §N.42.3):
                    // held for the host; refused at once when it can't be.
                    "idn_key_request" => {
                        if let Some(reply) = self.key_requested(&m, conn_id, &info) {
                            proto::send(&mut *w.lock_ok(), &reply.to_xml())?;
                        }
                    }
                    // A relayed request taken back by its relay (§N.42.3).
                    "idn_key_withdraw" => {
                        self.key_withdrawn(m.attr("req"), conn_id);
                    }
                    // The pairing code this relay shows (idn-node.md §N.42.3):
                    // made by the site, sent back.
                    "idn_pairing_code" => {
                        // Only to a machine marked as a pairing relay.
                        let reply = if self.is_relay(&info.fingerprint) {
                            let (code, until, left) = self.pairing_code_left(&info.fingerprint);
                            Element::new("idn_pairing_code_result")
                                .with_attr("req", m.attr("req").unwrap_or(""))
                                .with_attr("code", &code)
                                .with_attr("until", &until.to_string())
                                .with_attr("left", &left.to_string())
                        } else {
                            Element::new("idn_pairing_code_result")
                                .with_attr("req", m.attr("req").unwrap_or(""))
                                .with_attr(
                                    "error",
                                    "this machine isn't a pairing relay: mark it as one in the Site Manager",
                                )
                        };
                        proto::send(&mut *w.lock_ok(), &reply.to_xml())?;
                    }
                    // This machine leaves the site (idn-node.md §N.42).
                    "idn_leave" => {
                        if let Err(e) = self.left(&info) {
                            crate::log::log(&format!("site authority: a machine leaving: {e}"));
                        }
                        // (Its certificate revoked: the session ends here, its endpoint
                        // and reports with it.)
                        break;
                    }
                    // A named API key used on this device (idn-node.md §N.42).
                    "idn_key_used" => {
                        let key = m.attr("key").unwrap_or("");
                        if !key.is_empty()
                            && key.len() <= 64
                            && key.bytes().all(|b| b.is_ascii_alphanumeric())
                        {
                            self.event(AuthorityEvent::KeyUsed {
                                key: key.to_owned(),
                                machine: info.name.clone(),
                            });
                        }
                    }
                    "idn_site_transmitters" => {
                        let reply = site_transmitters(&self.node_reports());
                        proto::send(&mut *w.lock_ok(), &reply.to_xml())?;
                    }
                    // The directory of channel endpoints (§E.12.2).
                    "idn_endpoint" => {
                        let port = m.attr("port").and_then(|p| p.parse::<u16>().ok());
                        if let (Some(port @ 1..), Some(address)) = (port, address) {
                            let e = Endpoint {
                                info: info.clone(),
                                address,
                                port,
                            };
                            self.0.state.lock_ok().endpoints.insert(conn_id, e.clone());
                            self.directory_changed(vec![e.element()], conn_id);
                        } else if port == Some(0) {
                            // Withdrawn (its channel ended): gone from the directory unless
                            // another connection of the device still announces one.
                            let gone = {
                                let mut st = self.0.state.lock_ok();
                                st.endpoints.remove(&conn_id).filter(|e| {
                                    !st.endpoints
                                        .values()
                                        .any(|o| o.info.fingerprint == e.info.fingerprint)
                                })
                            };
                            if let Some(e) = gone {
                                self.directory_changed(
                                    vec![Element::new("gone").with_attr("fp", &e.info.fingerprint)],
                                    conn_id,
                                );
                            }
                        }
                    }
                    // An IDN Node's report (idn-node.md §N.11.2).
                    "idn_node_report" => {
                        self.0.state.lock_ok().node_reports.insert(
                            conn_id,
                            (info.fingerprint.clone(), m.clone(), Instant::now(), address),
                        );
                    }
                    // A Node's crash report (idn-node.md §N.16): kept once, by device.
                    "idn_node_crash" => {
                        if let Err(e) = keep_report(&self.0.dir, &info.fingerprint, &m) {
                            crate::log::log(&format!(
                                "site: a crash report from {}: {e}",
                                info.fingerprint
                            ));
                        }
                    }
                    // A program's alert, through its Node (idn-node.md §N.18).
                    "idn_node_alert" => {
                        if let Err(e) = keep_alert(&self.0.dir, &info.fingerprint, &m) {
                            crate::log::log(&format!(
                                "site: an alert from {}: {e}",
                                info.fingerprint
                            ));
                        }
                    }
                    "idn_node_set_result" => {
                        if let Some(id) = m.attr("id").and_then(|i| i.parse::<u64>().ok()) {
                            let mut st = self.0.state.lock_ok();
                            // Only the machine it was sent to, for a setting still waited for:
                            // any enrolled device could otherwise answer for another, or
                            // fill this with answers nobody waits for.
                            if st.node_pending.get(&id) == Some(&info.fingerprint) {
                                st.node_results.insert(id, m.clone());
                            }
                            drop(st);
                            let (_, cv) = &self.0.events;
                            cv.notify_all();
                        }
                    }
                    // A certificate for the machine's HTTPS ports (§E.10.11): one every
                    // 10 s at most a connection (signing isn't free).
                    "idn_lan_cert" => {
                        if last_lan.is_some_and(|t: Instant| t.elapsed() < LAN_EVERY) {
                            let reply = Element::new("idn_lan_cert")
                                .with_attr("error", "asked again too soon");
                            proto::send(&mut *w.lock_ok(), &reply.to_xml())?;
                            continue;
                        }
                        last_lan = Some(Instant::now());
                        let reply = match self.issue_lan(&cert, &info, &m, address) {
                            Ok(der) => Element::new("idn_lan_cert").with_attr("cert", &b64(&der)),
                            Err(e) => {
                                crate::log::log(&format!(
                                    "trust: no HTTPS certificate for {:?}: {e}",
                                    info.name
                                ));
                                Element::new("idn_lan_cert").with_attr("error", &e.to_string())
                            }
                        };
                        proto::send(&mut *w.lock_ok(), &reply.to_xml())?;
                    }
                    "idn_directory" => {
                        let list = {
                            let mut st = self.0.state.lock_ok();
                            st.directory_subs.insert(conn_id);
                            let mut list = Element::new("idn_directory_result")
                                .with_attr("devices", &super::device_count(&st).to_string());
                            list.children = st.endpoints.values().map(Endpoint::element).collect();
                            list
                        };
                        proto::send(&mut *w.lock_ok(), &list.to_xml())?;
                    }
                    _ => {}
                }
            }
            Ok(())
        })();
        self.drop_key_requests(Some(conn_id));
        let gone = {
            let mut st = self.0.state.lock_ok();
            st.online.retain(|(_, id, _)| *id != conn_id);
            st.node_reports.remove(&conn_id);
            st.directory_subs.remove(&conn_id);
            st.endpoints.remove(&conn_id).filter(|e| {
                // Another connection of the same device may still announce it.
                !st.endpoints
                    .values()
                    .any(|o| o.info.fingerprint == e.info.fingerprint)
            })
        };
        if let Some(e) = gone {
            self.directory_changed(
                vec![Element::new("gone").with_attr("fp", &e.info.fingerprint)],
                conn_id,
            );
        }
        let _ = raw.shutdown(std::net::Shutdown::Both);
        r
    }

    /// One remote administration request (§E.10.8).
    pub(super) fn admin(&self, m: &Element) -> Element {
        let op = m.attr("op").unwrap_or("");
        let res = Element::new("idn_admin_result").with_attr("op", op);
        let fail = |e: io::Error| {
            res.clone()
                .with_attr("ok", "false")
                .with_attr("error", &e.to_string())
        };
        let ok = |r: Element| r.with_attr("ok", "true");
        match op {
            "pairing" => {
                match m
                    .attr("seconds")
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(PAIRING_DEFAULT.as_secs())
                {
                    0 => self.close_pairing(),
                    n => self.open_pairing(Duration::from_secs(n)),
                }
                ok(res)
            }
            "pending" => {
                let mut r = res.clone();
                for p in self.pending() {
                    r.children.push(
                        Element::new("request")
                            .with_attr("id", &p.id.to_string())
                            .with_attr("name", &p.name)
                            .with_attr("kind", &p.kind)
                            .with_attr("fp", &p.fingerprint)
                            .with_attr("code", &p.code)
                            .with_attr(
                                "address",
                                &p.address.map(|a| a.to_string()).unwrap_or_default(),
                            ),
                    );
                }
                ok(r)
            }
            "devices" => {
                let mut r = res.clone();
                for d in self.devices() {
                    r.children.push(
                        Element::new("device")
                            .with_attr("fp", &d.fingerprint)
                            .with_attr("name", &d.name)
                            .with_attr("kind", &d.kind)
                            .with_attr("admin", &d.admin.to_string())
                            .with_attr("revoked", &d.revoked.to_string())
                            .with_attr("online", &d.online.to_string()),
                    );
                }
                ok(r)
            }
            "approve" | "deny" => match m.attr("id").and_then(|v| v.parse().ok()) {
                Some(id) => match if op == "approve" {
                    self.approve(id)
                } else {
                    self.deny(id)
                } {
                    Ok(()) => ok(res),
                    Err(e) => fail(e),
                },
                None => fail(err("missing id")),
            },
            "revoke" | "clear_denied" => match m.attr("fp") {
                Some(fp) => match if op == "revoke" {
                    self.revoke(fp)
                } else {
                    self.clear_denied(fp)
                } {
                    Ok(()) => ok(res),
                    Err(e) => fail(e),
                },
                None => fail(err("missing fp")),
            },
            "options" => {
                let mut r = res.clone();
                r.children.push(self.options().to_element());
                ok(r)
            }
            "set" | "unset" => {
                let scope = match (m.attr("fp"), m.attr("output"), m.attr("input")) {
                    (Some(fp), _, _) => crate::trust::options::OptionScope::Device(fp.to_owned()),
                    (_, Some(o), _) => crate::trust::options::OptionScope::Output(o.to_owned()),
                    (_, _, Some(i)) => crate::trust::options::OptionScope::Input(i.to_owned()),
                    _ => crate::trust::options::OptionScope::Site,
                };
                let Some(key) = m.attr("key") else {
                    return fail(err("missing key"));
                };
                let r = if op == "set" {
                    match m.attr("value") {
                        Some(v) => self.set_option(&scope, key, v),
                        None => Err(err("missing value")),
                    }
                } else {
                    self.unset_option(&scope, key)
                };
                match r {
                    Ok(version) => ok(res.with_attr("version", &version.to_string())),
                    Err(e) => fail(e),
                }
            }
            _ => fail(err("unknown op")),
        }
    }
}

/// The fields of a join by code (§E.10.10): the keys, the name and kind as sent (bound in
/// the exchange) and as kept, and where it came from.
pub(super) struct CodeJoiner<'a> {
    pub(super) spki: &'a [u8],
    pub(super) ca_spki: &'a [u8],
    pub(super) name_sent: &'a str,
    pub(super) kind_sent: &'a str,
    pub(super) name: &'a str,
    pub(super) kind: &'a str,
    pub(super) fp: &'a str,
    pub(super) peer: Option<IpAddr>,
}

/// `<idn_site_transmitters>` (§E.12.12): each machine's reported audio transmitters, with
/// their channel counts, labels and origins (§A.16, `origin_kind`, `origin_device`,
/// `origin_channels`, where the program said); at most 1024.
fn site_transmitters(reports: &[super::NodeReport]) -> Element {
    // Well under a trust message's limit (1 MiB): one machine's long labels made the reply
    // too long for every machine asking, ending their trust sessions (bug run 2026-11-24).
    const MAX_REPLY: usize = 512 * 1024;
    const MAX_LABELS: usize = 8 * 1024;
    let cut = |v: &str, n: usize| -> String {
        let mut end = v.len().min(n);
        while !v.is_char_boundary(end) {
            end -= 1;
        }
        v[..end].to_owned()
    };
    let mut out = Element::new("idn_site_transmitters");
    let mut size = 0usize;
    for r in reports {
        let machine = cut(r.report.attr("name").unwrap_or(""), 256);
        for t in r.report.children_named("audio_tx").take(1024) {
            if out.children.len() >= 1024 || size >= MAX_REPLY {
                return out;
            }
            let mut tx = Element::new("tx")
                .with_attr("name", &cut(t.attr("name").unwrap_or(""), 256))
                .with_attr("channels", &cut(t.attr("channels").unwrap_or("2"), 8))
                .with_attr("labels", &cut(t.attr("labels").unwrap_or(""), MAX_LABELS))
                .with_attr("machine", &machine);
            if let Some(o) = crate::node::routing::AudioOrigin::from_element(t) {
                for (k, v) in o.attrs() {
                    tx = tx.with_attr(k, &cut(&v, 256));
                }
            }
            size += tx.to_xml().len();
            out = out.with_child(tx);
        }
    }
    out
}

#[cfg(test)]
mod site_tx_tests {
    use crate::xml::Element;

    #[test]
    fn the_sites_transmitters_with_their_channels() {
        let report = |machine: &str, txs: &[(&str, &str)]| super::super::NodeReport {
            fingerprint: machine.into(),
            report: txs.iter().fold(
                Element::new("idn_node_report").with_attr("name", machine),
                |e, (n, ch)| {
                    e.with_child(
                        Element::new("audio_tx")
                            .with_attr("name", n)
                            .with_attr("channels", ch),
                    )
                },
            ),
            age: std::time::Duration::ZERO,
            address: None,
        };
        let l = super::site_transmitters(&[
            report("BOOTH", &[("BOOTH (Desk)", "64"), ("BOOTH (Mic)", "1")]),
            report("STAGE", &[("STAGE (Stagebox)", "32")]),
        ]);
        let got: Vec<(String, String, String)> = l
            .children_named("tx")
            .map(|t| {
                (
                    t.attr("name").unwrap().to_owned(),
                    t.attr("channels").unwrap().to_owned(),
                    t.attr("machine").unwrap().to_owned(),
                )
            })
            .collect();
        assert_eq!(got.len(), 3);
        assert_eq!(
            got[2],
            ("STAGE (Stagebox)".into(), "32".into(), "STAGE".into())
        );
        // A transmitter's origin (§A.16), where its program said it, goes with it.
        let mut with_origin = report("FOH", &[("FOH (Stage box)", "2")]);
        with_origin.report.children[0] = with_origin.report.children[0]
            .clone()
            .with_attr("origin_kind", "network_device")
            .with_attr(
                "origin_channels",
                "Stage Box Left/Mic 5,Stage Box Left/Mic 6",
            );
        let l = super::site_transmitters(&[with_origin]);
        let o = crate::node::routing::AudioOrigin::from_element(&l.children[0]).unwrap();
        assert_eq!(o.kind, "network_device");
        assert_eq!(o.channels[1], "Stage Box Left/Mic 6");
        assert!(crate::node::routing::AudioOrigin::from_element(&got_none()).is_none());
        fn got_none() -> crate::xml::Element {
            crate::xml::Element::new("tx").with_attr("name", "X")
        }
    }
}

/// A wrong (or used) admin code, told apart from a failure at the site, which the device
/// may retry (bug run B: a disk's `PermissionDenied` read as a wrong code).
#[derive(Debug)]
struct WrongAdminCode;

impl std::fmt::Display for WrongAdminCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not the admin code, or it was used")
    }
}

impl std::error::Error for WrongAdminCode {}
