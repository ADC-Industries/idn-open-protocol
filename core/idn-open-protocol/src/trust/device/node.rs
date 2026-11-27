//! A membership held by the IDN Node (§N.5): its requests, its identity, its events' thread.

use super::*;

impl Enrollment {
    /// The machine's membership as its IDN Node holds it (spec/extensions/idn-node.md
    /// §N.5): credentials, state, options and the directory come from the Node, which also
    /// signs for the device key. `identity` is the Node's `identity` reply.
    pub(crate) fn via_node(
        conn: Arc<crate::node::Conn>,
        identity: &Element,
        events: std::sync::mpsc::Receiver<Element>,
    ) -> io::Result<Arc<Self>> {
        let public = crate::node::public_attr(identity, "device_key")
            .ok_or_else(|| err("IDN Node sent no device key"))?;
        let signer: Arc<dyn super::super::pki::RemoteSigner> = conn.clone();
        let key = KeyFile::remote(public, signer, "device");
        let open = identity
            .attr("device_open_cert")
            .and_then(unb64)
            .map(|c| crate::link::Identity::remote(key.clone(), c));
        let e = Arc::new(Enrollment {
            node: Some(conn.clone()),
            node_interface: identity.attr("interface").and_then(|i| i.parse().ok()),
            open_identity: std::sync::OnceLock::new(),
            endpoint: Mutex::new(None),
            directory: Mutex::new(None),
            site_devices: Mutex::new(None),
            site_txs: Mutex::new((Vec::new(), None)),
            trust_writer: Mutex::new(None),
            cfg: EnrollmentConfig {
                discovery_server: identity.attr("server").unwrap_or("").to_owned(),
                state_dir: PathBuf::new(),
                name: identity.attr("name").unwrap_or("").to_owned(),
                role: DeviceRole::App,
                passphrase: None,
                admin_code: None,
            },
            key,
            site: RwLock::new(None),
            creds: RwLock::new(None),
            state: Mutex::new(TrustState::Starting),
            events: Default::default(),
            sessions: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            me: Default::default(),
            options: Mutex::new(Arc::new(Default::default())),
            options_ca: Mutex::new(String::new()),
            options_cv: Condvar::new(),
            admin_code_refused: AtomicBool::new(false),
            admin_replies: Default::default(),
            admin_next: Default::default(),
            key_answers: Mutex::new((1, Default::default())),
            pairing_codes: Default::default(),
            key_code_wrong: Default::default(),
            kick: AtomicBool::new(false),
            node_require: AtomicBool::new(false),
            node_sets: Default::default(),
            pairs: Default::default(),
            pair_uses: Default::default(),
            pair_sessions: Default::default(),
            confirm_codes: AtomicBool::new(false),
            confirmation: Default::default(),
            typed_code: Mutex::new(None),
            code_in_use: Mutex::new(None),
            site_mode: Mutex::new(None),
            lan: Default::default(),
        });
        if let Some(o) = open {
            let _ = e.open_identity.set(o);
        }
        let _ = e.me.set(Arc::downgrade(&e));
        e.apply_node_identity(identity);
        if let Ok(o) = conn.request(Element::new("options"), Duration::from_secs(5)) {
            e.apply_options(&o);
        }
        let weak = Arc::downgrade(&e);
        std::thread::Builder::new()
            .name("idn-trust-node".into())
            .spawn(move || run_node(weak, events))?;
        Ok(e)
    }

    /// Whether this membership is held by the IDN Node.
    pub fn via_node_endpoint(&self) -> bool {
        self.node.is_some()
    }

    /// The IDN Node's "require encryption" for the machine, now (false when the
    /// membership isn't the Node's).
    pub fn node_requires(&self) -> bool {
        self.node.is_some() && self.node_require.load(Ordering::Relaxed)
    }

    /// The machine settings a program takes from the IDN Node when it set none of its
    /// own (spec/extensions/idn-node.md §N.1): the device name, the Discovery Server and
    /// the network interface. `None` when the membership isn't the Node's.
    pub fn node_machine(&self) -> Option<NodeMachine> {
        self.node.as_ref()?;
        Some(NodeMachine {
            name: Some(self.cfg.name.clone()).filter(|n| !n.is_empty()),
            discovery_server: Some(self.cfg.discovery_server.clone()).filter(|s| !s.is_empty()),
            interface: self.node_interface,
        })
    }

    /// Tells the IDN Node about an output or input of this program (§N.4.2), when the
    /// membership is the Node's; listed in its panel until the value is dropped.
    pub fn node_register(
        &self,
        kind: &str,
        name: &str,
        port: Option<u16>,
    ) -> Option<crate::node::Registration> {
        crate::node::register_on(self.node.as_ref()?, kind, name, port).ok()
    }

    /// The site's options (§E.20): the latest document the authority sent, or the one
    /// kept from the last run (version 0 and empty if none yet). Resolve them with
    /// [`SiteOptions::for_output`](super::super::options::SiteOptions::for_output) and friends.
    pub fn options(&self) -> Arc<super::super::options::SiteOptions> {
        self.options.lock_ok().clone()
    }

    /// Waits up to `timeout` for a document newer than `since` (a version), and gives
    /// it; `None` if none came.
    pub fn wait_options(
        &self,
        since: u64,
        timeout: Duration,
    ) -> Option<Arc<super::super::options::SiteOptions>> {
        let end = Instant::now() + timeout;
        let mut cur = self.options.lock_ok();
        while cur.version <= since {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            cur = self
                .options_cv
                .wait_timeout(cur, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        Some(cur.clone())
    }

    /// Sends an administration request (§E.10.8, §E.20.5) over the trust connection and
    /// waits up to `timeout` for the answer. Needs the admin role and an enrolled,
    /// connected device; the authority answers `ok="false"` otherwise or not at all.
    pub fn admin(
        &self,
        request: &crate::xml::Element,
        timeout: Duration,
    ) -> io::Result<crate::xml::Element> {
        if request.name != "idn_admin" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not an idn_admin element",
            ));
        }
        if !matches!(self.state(), TrustState::Enrolled { connected: true }) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "not enrolled and connected to the authority",
            ));
        }
        if let Some(h) = &self.node {
            return h.request(Element::new("admin").with_child(request.clone()), timeout);
        }
        // Numbered, and only its own answer taken (bug run 2026-11-25: two callers at once,
        // or an answer arriving after its caller gave up, got each other's). An authority
        // from before echoes no number: its answers are taken in order, as they were.
        let n = self.admin_next.fetch_add(1, Ordering::Relaxed) + 1;
        // (Unnumbered answers, an older authority's, left from a caller that gave up:
        // dropped, as before, so this call doesn't take one; bug run B.)
        self.admin_replies
            .0
            .lock_ok()
            .retain(|r| r.attr("req").is_some());
        let request = request.clone().with_attr("req", &n.to_string());
        self.trust_send(&request.to_xml());
        let end = Instant::now() + timeout;
        let (q, cv) = &self.admin_replies;
        let mut q = q.lock_ok();
        loop {
            let mine = q.iter().position(|r| {
                r.attr("req")
                    .is_none_or(|x| x.parse::<u64>().ok() == Some(n))
            });
            if let Some(i) = mine {
                return Ok(q.remove(i).expect("found"));
            }
            // Answers nobody waits for any more (their callers gave up): the old ones go.
            q.retain(|r| {
                r.attr("req")
                    .and_then(|x| x.parse::<u64>().ok())
                    .is_none_or(|x| x + 64 > n)
            });
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "no answer in time"));
            }
            q = cv
                .wait_timeout(q, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    /// The next settings message the authority sent for the IDN Node (idn-node.md
    /// §N.11.2), waiting up to `timeout`.
    #[doc(hidden)]
    pub fn next_node_set(&self, timeout: Duration) -> Option<Element> {
        let (q, cv) = &self.node_sets;
        let mut q = q.lock_ok();
        if q.is_empty() {
            q = cv
                .wait_timeout(q, timeout)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        q.pop_front()
    }

    /// Sends a message to the authority over the trust connection, if connected (the IDN
    /// Node's reports and answers, idn-node.md §N.11.2).
    #[doc(hidden)]
    pub fn send_to_authority(&self, e: &Element) {
        self.trust_send(&e.to_xml());
    }

    /// The Node's `identity` (§N.4.3): state, credentials, CRL and passphrase key.
    fn apply_node_identity(&self, id: &Element) {
        let Some(node) = self.node.clone() else {
            return;
        };
        crate::node::video::heard(id);
        crate::node::rate::heard(id);
        // "Require encryption", live: a change reached running programs only when they
        // started again (an NDI receiver that can't encrypt still got a source's
        // pictures).
        self.node_require
            .store(id.attr("require") == Some("true"), Ordering::Relaxed);
        // Linked computers (§E.28): only while the Node says they are on (not in a site).
        let pairs: Vec<super::super::pair::Linked> = id
            .child("pairs")
            .filter(|p| p.attr("on") == Some("true"))
            .map(|p| {
                p.children_named("pair")
                    .filter_map(super::super::pair::Linked::from_element)
                    .collect()
            })
            .unwrap_or_default();
        if **self.pairs.read_ok() != pairs {
            *self.pairs.write_ok() = Arc::new(pairs);
            self.close_unlinked();
        }

        // The passphrase key, when it changed.
        let key_id = id.attr("key_id").filter(|k| !k.is_empty());
        let cur = self.site_key().map(|k| k.key_id.clone());
        if key_id != cur.as_deref() {
            let next = match (key_id, id.attr("site_cert").and_then(unb64)) {
                (Some(k), Some(cert)) => {
                    let signer: Arc<dyn super::super::pki::RemoteSigner> = node.clone();
                    SiteKey::remote(k, cert, signer).ok()
                }
                _ => None,
            };
            *self.site.write_ok() = next;
        }
        // The device certificate, CA and CRL.
        let cert = id.attr("cert").and_then(unb64);
        let ca = id.attr("ca").and_then(unb64);
        match (cert, ca) {
            (Some(cert), Some(ca)) => {
                let same = self
                    .creds()
                    .is_some_and(|c| c.cert_der == cert && c.anchor.ca_der == ca);
                if !same && let Ok(anchor) = Anchor::new(ca) {
                    let creds = Arc::new(DeviceCreds {
                        key: self.key.clone(),
                        cert_der: cert,
                        anchor: Arc::new(anchor),
                    });
                    *self.creds.write_ok() = Some(creds);
                }
                if let (Some(c), Some(crl)) = (self.creds(), id.attr("crl").and_then(unb64)) {
                    let _ = self.apply_crl(&c, &crl);
                }
                self.apply_node_lan_cert(id);
            }
            _ => {
                // Not enrolled (any more): no device credentials, and live sessions with
                // enrolled peers end (§E.10.6).
                if self.creds().is_some() {
                    self.forget_creds();
                    for (_, s) in self.sessions.lock_ok().drain(..) {
                        let _ = s.shutdown(std::net::Shutdown::Both);
                    }
                }
            }
        }
        let connected = id.attr("connected") == Some("true");
        let st = match id.attr("state").unwrap_or("") {
            "enrolled" if self.creds().is_some() => TrustState::Enrolled { connected },
            "pending" => TrustState::Pending {
                code: id.attr("code").unwrap_or("").to_owned(),
            },
            "confirm" => TrustState::ConfirmCode {
                code: id.attr("code").unwrap_or("").to_owned(),
            },
            "waiting" => TrustState::WaitingForPairing,
            "code_needed" => TrustState::CodeNeeded,
            "code_accepted" => TrustState::CodeAccepted,
            "wrong_code" => TrustState::WrongCode {
                left: id
                    .attr("code_left")
                    .and_then(|l| l.parse().ok())
                    .unwrap_or(0),
            },
            "code_expired" => TrustState::CodeExpired,
            "denied" => TrustState::Denied,
            "revoked" => TrustState::Revoked,
            "no_authority" | "none" => TrustState::NoAuthority,
            "error" => TrustState::Error(id.attr("error").unwrap_or("error").to_owned()),
            _ => TrustState::Starting,
        };
        self.set(st);
    }
}

/// The thread of a membership held by the IDN Node: its events, and catching up after the
/// Node restarts (§N.5).
fn run_node(weak: std::sync::Weak<Enrollment>, events: std::sync::mpsc::Receiver<Element>) {
    loop {
        let m = match events.recv_timeout(Duration::from_secs(1)) {
            Ok(m) => m,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => match weak.upgrade() {
                Some(e) if !e.abandoned() => continue,
                _ => return,
            },
            Err(_) => return,
        };
        let Some(e) = weak.upgrade() else { return };
        if e.abandoned() {
            return;
        }
        let Some(node) = e.node.clone() else { return };
        match m.name.as_str() {
            "identity" => e.apply_node_identity(&m),
            "idn_options" => e.apply_options(&m),
            "idn_directory_result" => e.apply_directory(&m, true),
            "idn_directory_change" => e.apply_directory(&m, false),
            "clock" => crate::node::clock::heard(&m),
            "node_lost" => {
                crate::log::log("IDN Node: connection lost; reconnecting");
                // Sessions go on; new handshakes wait for the Node (it signs them).
                e.set(TrustState::Error("IDN Node not running".into()));
            }
            "node_connected" => {
                crate::log::log("IDN Node: connected again");
                if let Ok(id) = node.request(Element::new("identity"), Duration::from_secs(5)) {
                    e.apply_node_identity(&id);
                }
                if let Ok(o) = node.request(Element::new("options"), Duration::from_secs(5)) {
                    e.apply_options(&o);
                }
                if let Ok(c) = node.request(Element::new("clock"), Duration::from_secs(5))
                    && c.name == "clock"
                {
                    crate::node::clock::heard(&c);
                }
                if let Some(port) = *e.endpoint.lock_ok() {
                    node.send(Element::new("endpoint").with_attr("port", &port.to_string()));
                }
                if e.directory.lock_ok().is_some() {
                    node.send(Element::new("directory"));
                }
            }
            _ => {}
        }
    }
}
