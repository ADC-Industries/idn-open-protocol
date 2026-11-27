//! The device's trust connection with its site's authority (§E.10.2): the hello, a join
//! (with the passphrase, a typed code or approval), and an enrolled device's TLS session
//! (CRLs, options, the directory, the admin code, its LAN certificate).

use super::*;

impl Enrollment {
    /// Hello and `idn_trust_ok` (§E.10.2): the CA certificate, or `None` if the server is
    /// not an authority (no answer within 2 s, §E.10.3).
    pub(super) fn hello(&self, server: SocketAddr) -> io::Result<Option<(TcpStream, Vec<u8>)>> {
        let mut s = trust_socket(server)?;
        // Version 2: told the site's join mode, and may join with a typed code (§E.10.10).
        proto::send(&mut s, r#"<idn_trust version="2"/>"#)?;
        s.set_read_timeout(Some(Duration::from_secs(2)))?;
        match proto::recv_into(&mut s, &mut Vec::new(), proto::MAX_PLAIN) {
            Ok(Some(m)) if m.name == "idn_trust_ok" => {
                let ca = m
                    .attr("ca")
                    .and_then(unb64)
                    .ok_or_else(|| err("idn_trust_ok without a CA"))?;
                *self.site_mode.lock_ok() = m.attr("join").and_then(crate::trust::JoinMode::parse);
                Ok(Some((s, ca)))
            }
            Ok(_) => Ok(None),
            Err(e) if crate::transport::is_poll_timeout(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The next trust message from `r` (which has a short read timeout), waiting until
    /// `deadline` if any, and giving up when the enrollment is abandoned.
    pub(super) fn recv_while_wanted(
        &self,
        r: &mut impl std::io::Read,
        partial: &mut Vec<u8>,
        max: usize,
        deadline: Option<Instant>,
    ) -> io::Result<Option<crate::xml::Element>> {
        loop {
            match proto::recv_into(r, partial, max) {
                Err(e) if crate::transport::is_poll_timeout(&e) => {
                    if self.abandoned() {
                        return Err(err("enrollment no longer used"));
                    }
                    // A code typed while a join waits: that join gives way (§E.10.10).
                    if self.creds().is_none() && {
                        let typed = self.typed_code.lock_ok().clone();
                        typed.is_some() && typed != *self.code_in_use.lock_ok()
                    } {
                        return Err(io::Error::other(super::CodeTyped));
                    }
                    if deadline.is_some_and(|d| Instant::now() >= d) {
                        return Err(e);
                    }
                    // The HTTPS certificate renewed, or asked for anew when the
                    // machine's names or addresses changed (§E.10.11).
                    self.lan_tick();
                }
                other => return other,
            }
        }
    }

    pub(super) fn join(&self, server: SocketAddr) -> io::Result<JoinOutcome> {
        let Some((mut s, ca_der)) = self.hello(server)? else {
            return Ok(JoinOutcome::NoAuthority);
        };
        let anchor = Anchor::new(ca_der.clone())?;
        let ca_spki = {
            let (_, c) = x509_parser::parse_x509_certificate(&ca_der).map_err(err)?;
            c.public_key().raw.to_vec()
        };
        let spki = self.key.spki();
        let mut join = Element::new("idn_join")
            .with_attr("name", &self.cfg.name)
            .with_attr("kind", self.cfg.role.as_str())
            .with_attr("key", &b64(&spki));
        // A typed code (§E.10.10), where the site takes codes; used up by the answer (the
        // caller, `run`). A site that doesn't gets the usual join.
        let typed = self.typed_code.lock_ok().clone();
        *self.code_in_use.lock_ok() = typed.clone();
        let mut pake = match (typed, self.site_join_mode()) {
            (Some(code), Some(m)) if m.takes_codes() => {
                let side = crate::trust::join_code::DeviceSide::new(&code, &spki, &ca_spki);
                join = join
                    .with_attr("nd", &b64(&side.nd))
                    .with_attr("pake", &b64(&side.x));
                Some(side)
            }
            (Some(_), _) => {
                crate::log::log(
                    "trust: this site doesn't take join codes: asking for approval instead",
                );
                None
            }
            _ => None,
        };
        // Proven by the typed code: the authority's confirmation matched.
        let mut by_code = false;
        let site_key = self.site_key();
        if let Some(site) = &site_key {
            // The passphrase proof (§E.10.4): over the name, kind and time.
            let at = idn_common::epoch::secs();
            join = join
                .with_attr("psk_at", &at.to_string())
                .with_attr(
                    "psk_mac2",
                    &b64(&site.join_mac2(
                        &spki,
                        &ca_spki,
                        &self.cfg.name,
                        self.cfg.role.as_str(),
                        at,
                    )),
                );
        }
        proto::send(&mut s, &join.to_xml())?;
        // Short reads, so that a dropped enrollment is noticed; answers are due within 10 s
        // until the join is pending (the operator may take a while).
        s.set_read_timeout(Some(Duration::from_secs(1)))?;
        let mut deadline = Some(Instant::now() + Duration::from_secs(10));
        let mut partial = Vec::new();
        let mut verified_by_psk = false;
        let mut code = None::<String>;
        loop {
            let m = self
                .recv_while_wanted(&mut s, &mut partial, proto::MAX_PLAIN, deadline)?
                .ok_or_else(|| err("authority closed the connection"))?;
            // 10 s for each answer, not for the whole exchange (the site saves and signs
            // between them: a busy disk took longer).
            if deadline.is_some() {
                deadline = Some(Instant::now() + Duration::from_secs(10));
            }
            match m.name.as_str() {
                "idn_commit" => {
                    let c = m
                        .attr("c")
                        .and_then(unb64)
                        .ok_or_else(|| err("bad commitment"))?;
                    let nd = crate::random::secure_random::<32>();
                    proto::send(&mut s, &format!(r#"<idn_nonce n="{}"/>"#, b64(&nd)))?;
                    let r = self
                        .recv_while_wanted(&mut s, &mut partial, proto::MAX_PLAIN, deadline)?
                        .filter(|e| e.name == "idn_reveal")
                        .ok_or_else(|| err("expected idn_reveal"))?;
                    let na = r
                        .attr("n")
                        .and_then(unb64)
                        .ok_or_else(|| err("bad reveal"))?;
                    let na32: [u8; 32] = na
                        .as_slice()
                        .try_into()
                        .map_err(|_| err("bad reveal length"))?;
                    if !proto::same(&proto::commit(&na32), &c) {
                        return Err(err(
                            "the authority's reveal does not match its commitment: possible attack",
                        ));
                    }
                    let k = proto::code(&spki, &ca_spki, &na, &nd);
                    code = Some(k);
                }
                // The authority's runs, one per code it holds (§E.10.10).
                // (One per join: each run answered is a guess at the code, and an authority
                // sending challenge after challenge would guess on until the deadline.)
                "idn_code_challenge" => {
                    let side = pake
                        .take()
                        .ok_or_else(|| err("a code challenge unasked, or a second one"))?;
                    let na = m
                        .attr("na")
                        .and_then(unb64)
                        .filter(|n| n.len() == 32)
                        .ok_or_else(|| err("bad code challenge"))?;
                    let runs: Vec<(Vec<u8>, Vec<u8>)> = m
                        .children_named("run")
                        .filter_map(|r| {
                            Some((
                                r.attr("y").and_then(unb64)?,
                                r.attr("confirm").and_then(unb64)?,
                            ))
                        })
                        .collect();
                    // More runs than a site holds codes: each would be a guess at the code.
                    if runs.is_empty() || runs.len() > crate::trust::join_code::MAX_CODES {
                        return Err(err("a code challenge with too many runs: possible attack"));
                    }
                    let mine = side.check(
                        &spki,
                        &ca_spki,
                        &self.cfg.name,
                        self.cfg.role.as_str(),
                        &na,
                        &runs,
                    );
                    let mut reply = Element::new("idn_code_confirm");
                    if let Some(d) = mine {
                        reply = reply.with_attr("confirm", &b64(&d));
                        by_code = true;
                    }
                    proto::send(&mut s, &reply.to_xml())?;
                }
                "idn_join_status" => match m.attr("state") {
                    Some("pending") if by_code => {
                        self.set(TrustState::CodeAccepted);
                        deadline = None; // the operator may take a while
                    }
                    Some("pending") => {
                        let code = code.clone().ok_or_else(|| err("pending without a code"))?;
                        self.set(TrustState::Pending { code });
                        deadline = None; // the operator may take a while
                    }
                    Some("wrong_code") => {
                        let left = m.attr("left").and_then(|l| l.parse().ok()).unwrap_or(0);
                        return Ok(JoinOutcome::WrongCode(left));
                    }
                    Some("code_expired") => return Ok(JoinOutcome::CodeExpired),
                    Some("code_needed") => return Ok(JoinOutcome::CodeNeeded),
                    Some("code_limited") => {
                        let secs = m.attr("retry").and_then(|l| l.parse().ok()).unwrap_or(600);
                        return Ok(JoinOutcome::CodeLimited(secs));
                    }
                    Some("approved") if by_code => {}
                    Some("approved") => {
                        if code.is_none() {
                            // Auto-approval skips the code: the authority must prove the
                            // passphrase before its CA is trusted (§E.10.4).
                            let (Some(site), Some(mac)) =
                                (&site_key, m.attr("psk_mac").and_then(unb64))
                            else {
                                return Err(err("approved without a code or passphrase proof"));
                            };
                            if !proto::same(&mac, &site.join_mac("authority", &spki, &ca_spki)) {
                                return Err(err(
                                    "the authority's passphrase proof is wrong: not trusting it",
                                ));
                            }
                            verified_by_psk = true;
                        }
                    }
                    Some("closed") => return Ok(JoinOutcome::Closed),
                    Some("denied") => return Ok(JoinOutcome::Denied),
                    other => return Err(err(format!("unexpected join state {other:?}"))),
                },
                "idn_cert" => {
                    if code.is_none() && !verified_by_psk && !by_code {
                        return Err(err("certificate before approval"));
                    }
                    let cert = m
                        .attr("cert")
                        .and_then(unb64)
                        .ok_or_else(|| err("bad certificate"))?;
                    let info = anchor.verify(&cert)?;
                    if info.fingerprint != self.key.fingerprint() {
                        return Err(err("the certificate is not for this device"));
                    }
                    // Joined by comparison code: this device's person confirms the code
                    // first (the authority's approval alone proves nothing of a rogue
                    // authority). A typed code proved the site already.
                    if !verified_by_psk && !by_code && self.confirm_codes.load(Ordering::Relaxed) {
                        let code = code.clone().unwrap_or_default();
                        *self.confirmation.0.lock_ok() = None;
                        self.set(TrustState::ConfirmCode { code });
                        let (lock, cv) = &self.confirmation;
                        let end = std::time::Instant::now() + Duration::from_secs(600);
                        let mut a = lock.lock_ok();
                        while a.is_none() && !self.stop.load(Ordering::Relaxed) {
                            let left = end.saturating_duration_since(std::time::Instant::now());
                            if left.is_zero() {
                                break;
                            }
                            a = cv
                                .wait_timeout(a, left.min(Duration::from_secs(1)))
                                .unwrap()
                                .0;
                        }
                        if *a != Some(true) {
                            // The authority takes its certificate back (§E.10.4).
                            drop(a);
                            let _ = proto::send(&mut s, "<idn_join_withdraw/>");
                            return Ok(JoinOutcome::Unconfirmed);
                        }
                    }
                    // Pin the CA (verified by the code, the typed code or the passphrase
                    // proof).
                    crate::trust::pki::write_private(&self.path("ca.crt"), &ca_der)?;
                    crate::trust::pki::write_private(&self.path("device.crt"), &cert)?;
                    self.load_creds();
                    return Ok(JoinOutcome::Enrolled);
                }
                _ => {}
            }
        }
    }

    /// The certificate with the admin role, answering this device's admin code: kept in
    /// place of the one it has.
    pub(super) fn take_admin_cert(&self, creds: &DeviceCreds, m: &Element) -> io::Result<()> {
        let cert = m
            .attr("cert")
            .and_then(unb64)
            .ok_or_else(|| err("bad certificate"))?;
        let info = creds.anchor.verify(&cert)?;
        if info.fingerprint != self.key.fingerprint() || !info.admin {
            return Err(err("not this device's admin certificate"));
        }
        crate::trust::pki::write_private(&self.path("device.crt"), &cert)?;
        self.load_creds();
        Ok(())
    }

    /// The TLS trust connection of an enrolled device: CRLs and revocation (§E.10.6).
    pub(super) fn session(
        &self,
        server: SocketAddr,
        mut creds: Arc<DeviceCreds>,
    ) -> io::Result<()> {
        let Some((mut s, ca_der)) = self.hello(server)? else {
            return Err(err("authority not answering"));
        };
        if crate::trust::pki::fingerprint(&Anchor::new(ca_der)?.ca_der_spki()?)
            != creds.anchor.ca_fp
        {
            return Err(err(
                "this Discovery Server hosts a different site authority",
            ));
        }
        proto::send(&mut s, "<idn_starttls/>")?;
        s.set_read_timeout(Some(Duration::from_secs(5)))?;
        match proto::recv_into(&mut s, &mut Vec::new(), proto::MAX_PLAIN)? {
            Some(m) if m.name == "idn_starttls_ok" => {}
            _ => return Err(err("authority did not accept idn_starttls")),
        }
        s.set_read_timeout(None)?;
        let conn = rustls::ClientConnection::new(
            Arc::new(creds.client_config_for(proto::TRUST_ALPN, Some("authority"))?),
            "idn-site".try_into().map_err(err)?,
        )
        .map_err(err)?;
        let tls = crate::secure::wrap(s, rustls::Connection::Client(conn))?;
        // The one-time admin code goes inside TLS (§E.10.8), answered by a certificate with
        // the admin role; until the answer (or 10 s) this device isn't shown connected.
        let claim = self
            .cfg
            .admin_code
            .clone()
            .filter(|_| !creds.anchor.verify(&creds.cert_der).is_ok_and(|i| i.admin))
            .filter(|_| !self.admin_code_refused.load(Ordering::Relaxed));
        let mut claiming = claim
            .as_ref()
            .map(|_| Instant::now() + Duration::from_secs(10));
        // The channel directory (§E.12.2): announce and subscribe again on every
        // connection.
        *self.trust_writer.lock_ok() = tls.try_clone().ok();
        struct ClearWriter<'a>(&'a Mutex<Option<TcpStream>>);
        impl Drop for ClearWriter<'_> {
            fn drop(&mut self) {
                *self.0.lock_ok() = None;
            }
        }
        let _clear = ClearWriter(&self.trust_writer);
        // Connected once messages can go (a request made at once isn't lost).
        if claiming.is_none() {
            self.set(TrustState::Enrolled { connected: true });
        }
        if let Some(port) = *self.endpoint.lock_ok() {
            self.trust_send(&format!(r#"<idn_endpoint port="{port}"/>"#));
        }
        if self.directory.lock_ok().is_some() {
            self.trust_send("<idn_directory/>");
        }
        if let Some(code) = &claim {
            self.trust_send(
                &Element::new("idn_admin_claim")
                    .with_attr("code", code)
                    .to_xml(),
            );
        }
        // The certificate for the machine's HTTPS ports, if it has none or it changed.
        self.lan_session_started();
        // Inside TLS: buffered reads with a short timeout, to notice a dropped
        // enrollment (TR-4).
        tls.set_read_timeout(Some(Duration::from_secs(1)))?;
        let mut rd = std::io::BufReader::new(tls.try_clone()?);
        let mut partial = Vec::new();
        loop {
            let m =
                match self.recv_while_wanted(&mut rd, &mut partial, proto::MAX_MESSAGE, claiming) {
                    Ok(Some(m)) => m,
                    Ok(None) => break,
                    Err(_) if self.abandoned() => break,
                    Err(e) if claiming.is_some() && crate::transport::is_poll_timeout(&e) => {
                        crate::log::log("trust: the admin code had no answer");
                        claiming = None;
                        self.set(TrustState::Enrolled { connected: true });
                        continue;
                    }
                    Err(e) => return Err(e),
                };
            match m.name.as_str() {
                "idn_cert" if claiming.is_some() => {
                    if let Err(e) = self.take_admin_cert(&creds, &m) {
                        crate::log::log(&format!("trust: the admin certificate: {e}"));
                    }
                    // The credentials loaded with it from now on: CRLs go to the anchor
                    // everything else uses, and our own revocation is looked for under
                    // the new serial (bug run 2026-11-24).
                    if let Some(c) = self.creds() {
                        creds = c;
                    }
                    claiming = None;
                    self.set(TrustState::Enrolled { connected: true });
                }
                "idn_admin_claim_result" if claiming.is_some() => {
                    crate::log::log(&format!(
                        "trust: the admin code was refused: {}",
                        m.attr("error").unwrap_or("")
                    ));
                    claiming = None;
                    // (Only a wrong code: a failure at the site is tried at the next
                    // connection.)
                    if m.attr("retry") != Some("true") {
                        self.admin_code_refused.store(true, Ordering::Relaxed);
                    }
                    self.set(TrustState::Enrolled { connected: true });
                }
                "idn_crl" => {
                    let Some(der) = m.attr("crl").and_then(unb64) else {
                        continue;
                    };
                    // Our own serial in a signed CRL: revoked, even if the notice is lost.
                    if !self.apply_crl(&creds, &der)? {
                        return Ok(());
                    }
                }
                "idn_directory_result" => self.apply_directory(&m, true),
                "idn_site_transmitters" => {
                    let l = m
                        .children_named("tx")
                        .take(1024)
                        .filter_map(|t| {
                            Some(super::SiteTx {
                                name: t.attr("name")?.to_owned(),
                                channels: t
                                    .attr("channels")
                                    .and_then(|c| c.parse().ok())
                                    .unwrap_or(2),
                                machine: t.attr("machine").unwrap_or("").to_owned(),
                                origin: crate::node::routing::AudioOrigin::from_element(t),
                            })
                        })
                        .collect();
                    self.site_txs.lock_ok().0 = l;
                }
                "idn_directory_change" => self.apply_directory(&m, false),
                "idn_lan_cert" => self.take_lan_cert(&creds, &m),
                // The site's options (§E.20.4): only a newer document replaces the one held.
                "idn_options" => self.apply_options(&m),
                // A relayed pairing's answer (idn-node.md §N.42.3).
                "idn_key_result" => self.take_key_result(&m),
                // A relayed pairing's typed code refused (it still waits).
                "idn_key_status" => self.take_key_status(&m),
                // The pairing code this relay shows (idn-node.md §N.42.3).
                "idn_pairing_code_result" => self.take_pairing_code(&m),
                "idn_admin_result" => {
                    let (q, cv) = &self.admin_replies;
                    let mut q = q.lock_ok();
                    if q.len() < 64 {
                        q.push_back(m.clone());
                    }
                    cv.notify_all();
                }
                "idn_node_set" => {
                    let (q, cv) = &self.node_sets;
                    let mut q = q.lock_ok();
                    if q.len() < 64 {
                        q.push_back(m.clone());
                    }
                    cv.notify_all();
                }
                "idn_revoked" => {
                    self.forget_creds();
                    self.set(TrustState::Revoked);
                    return Ok(());
                }
                _ => {}
            }
        }
        Ok(())
    }
}
