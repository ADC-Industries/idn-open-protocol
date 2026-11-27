//! Opening the authority and keeping its state on disk: the CA, the device list, the CRL.

use super::*;

impl Authority {
    /// Opens the authority in `state_dir`, creating the site CA on first use.
    pub fn open(cfg: AuthorityConfig) -> io::Result<Self> {
        let dir = cfg.state_dir;
        std::fs::create_dir_all(&dir)?;
        let ca = Ca::from_key(KeyFile::load_or_create(&dir.join("ca.key"))?)?;
        let anchor = Arc::new(Anchor::new(ca.cert_der())?);
        // The authority's own TLS identity on trust connections, issued by the site CA.
        let skey = KeyFile::load_or_create(&dir.join("server.key"))?;
        let (scert, _) = ca.issue(&skey.spki(), "IDN site authority", "authority", false)?;
        let server = Arc::new(DeviceCreds {
            key: skey,
            cert_der: scert,
            anchor: anchor.clone(),
        });
        let site = cfg
            .passphrase
            .as_deref()
            .filter(|p| !p.is_empty())
            .map(SiteKey::from_passphrase)
            .transpose()?;
        // The device and denied lists: none yet is a new site; there but unreadable stops
        // the authority (read as empty, the CRL issued at once said nothing was revoked, a
        // revoked machine accepted again; bug round 2026-11-09).
        let read_list = |name: &str| -> io::Result<Option<String>> {
            match std::fs::read_to_string(dir.join(name)) {
                Ok(t) => Ok(Some(t)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(io::Error::new(
                    e.kind(),
                    format!("{}: {e}", dir.join(name).display()),
                )),
            }
        };
        let devices_text = read_list("devices.tsv")?;
        let denied_text = read_list("denied.txt")?;
        let relays_text = read_list("relays.txt")?;
        // The site's options likewise: read as none, a revoked API key's "not before" was
        // gone (the key worked again) and the version started over below what machines
        // hold, so they ignored every change (bug run 4).
        let options = match read_list("options.xml")? {
            None => Default::default(),
            Some(t) => super::super::options::SiteOptions::from_xml(&t).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: can't be read", dir.join("options.xml").display()),
                )
            })?,
        };
        let st = State {
            devices: devices_text
                .as_deref()
                .map(parse_devices)
                .unwrap_or_default(),
            denied: denied_text
                .map(|t| t.lines().map(str::to_owned).collect())
                .unwrap_or_default(),
            relays: relays_text
                .map(|t| {
                    t.lines()
                        .map(str::trim)
                        .filter(|l| !l.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            // (One that can't be read is set aside and said; the next CRL's number is at
            // least the time in ms anyway, so it still grows.)
            crl_number: idn_common::fsutil::load_saved(&dir.join("crl.number"), |b| {
                std::str::from_utf8(b).ok()?.trim().parse().ok()
            })
            .unwrap_or_else(|why| {
                crate::log::log(&format!("site authority: {why}"));
                None
            })
            .unwrap_or(0),
            crl_der: Vec::new(),
            pairing_until: None,
            pending: BTreeMap::new(),
            next_id: 1,
            proofs_seen: BTreeMap::new(),
            next_conn: 1,
            online: Vec::new(),
            endpoints: BTreeMap::new(),
            directory_subs: BTreeSet::new(),
            admin_code: Some(new_admin_code()),
            admin_tries: 0,
            options,
            node_reports: BTreeMap::new(),
            node_results: BTreeMap::new(),
            node_pending: BTreeMap::new(),
            next_set: 1,
            codes: super::super::join_code::Codes::load(&dir),
            key_requests: BTreeMap::new(),
            next_key_request: 1,
            key_request_life: key_requests::KEY_REQUEST_LIFE,
            pairing_codes: BTreeMap::new(),
            pairing_code_life: key_requests::PAIRING_CODE_LIFE,
            key_answered: Vec::new(),
            key_unclaimed: Vec::new(),
        };
        let a = Authority(Arc::new(Inner {
            dir,
            ca,
            anchor,
            server,
            site,
            state: Mutex::new(st),
            events: Default::default(),
            crl: Mutex::new(()),
        }));
        a.reissue_crl()?;
        Ok(a)
    }

    pub(super) fn save(&self) -> io::Result<()> {
        let st = self.0.state.lock_ok();
        let mut t = String::new();
        for r in &st.devices {
            let i = &r.info;
            t += &format!(
                "{}\t{}\t{}\t{}\t{}\t{}\n",
                idn_common::hex::hex(&i.serial),
                i.fingerprint,
                i.kind,
                u8::from(i.admin),
                // 2: withdrawn (revoked as well).
                if r.withdrawn { 2 } else { u8::from(r.revoked) },
                i.name.replace(['\t', '\n'], " ")
            );
        }
        super::super::pki::write_private(&self.0.dir.join("devices.tsv"), t.as_bytes())?;
        let d: String = st.denied.iter().map(|f| format!("{f}\n")).collect();
        super::super::pki::write_private(&self.0.dir.join("denied.txt"), d.as_bytes())?;
        let r: String = st.relays.iter().map(|f| format!("{f}\n")).collect();
        super::super::pki::write_private(&self.0.dir.join("relays.txt"), r.as_bytes())
    }

    /// Issues the next CRL with every revoked serial, installs it and pushes it.
    pub(super) fn reissue_crl(&self) -> io::Result<()> {
        // Two revocations at once (the UI and a remote admin) used to finish in either
        // order, leaving the older CRL as the one sent to reconnecting devices.
        let made = self.0.crl.lock_ok();
        let (number, serials) = {
            let mut st = self.0.state.lock_ok();
            // Devices ignore a CRL whose number isn't higher than the last they saw. The
            // stored counter can go back (a restored backup, a damaged crl.number), the
            // clock doesn't: numbers are also at least the time in milliseconds.
            let now_ms = idn_common::epoch::millis();
            st.crl_number = (st.crl_number + 1).max(now_ms);
            (
                st.crl_number,
                st.devices
                    .iter()
                    .filter(|r| r.revoked)
                    .map(|r| r.info.serial.clone())
                    .collect::<Vec<_>>(),
            )
        };
        let der = self.0.ca.crl(number, &serials)?;
        self.0.anchor.accept_crl(&der)?;
        super::super::pki::write_private(
            &self.0.dir.join("crl.number"),
            number.to_string().as_bytes(),
        )?;
        let msg = format!(r#"<idn_crl crl="{}"/>"#, b64(&der));
        // Written after releasing the state lock: one stalled device must not hold up
        // the authority (joins, approvals, the UI). Writes are bounded by WRITE_TIMEOUT.
        let online = {
            let mut st = self.0.state.lock_ok();
            st.crl_der = der;
            st.online.clone()
        };
        drop(made);
        let failed: Vec<u64> = online
            .iter()
            .filter(|(_, _, w)| proto::send(&mut *w.lock_ok(), &msg).is_err())
            .map(|(_, id, _)| *id)
            .collect();
        if !failed.is_empty() {
            let mut st = self.0.state.lock_ok();
            st.online.retain(|(_, id, _)| !failed.contains(id));
        }
        Ok(())
    }
}

fn parse_devices(t: &str) -> Vec<Record> {
    t.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.splitn(6, '\t').collect();
            let [serial, fp, kind, admin, revoked, name] = f.as_slice() else {
                return None;
            };
            Some(Record {
                info: CertInfo {
                    serial: idn_common::hex::unhex(serial)?,
                    fingerprint: (*fp).into(),
                    name: display_name(name),
                    kind: (*kind).into(),
                    admin: *admin == "1",
                },
                revoked: *revoked != "0",
                withdrawn: *revoked == "2",
            })
        })
        .collect()
}
