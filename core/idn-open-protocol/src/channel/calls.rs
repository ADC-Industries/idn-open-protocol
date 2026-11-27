//! The application's calls on a channel: starting it, messages, requests, fetches and
//! pushes, the transfer cap and trouble response, and its status.

use super::*;

impl Channel {
    /// Starts the endpoint, announces it to the authority and subscribes to the
    /// directory. Fails with `PermissionDenied` while the device isn't enrolled: without
    /// site trust there is no channel (§E.12.1).
    pub fn start(cfg: ChannelConfig, handler: Arc<dyn Handler>) -> io::Result<Arc<Self>> {
        let creds = cfg.enrollment.creds().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the channel needs site trust: this device is not enrolled",
            )
        })?;
        let mut server = creds.server_config(ALPN)?;
        server.max_early_data_size = 0;
        let server =
            quinn::crypto::rustls::QuicServerConfig::try_from(server).map_err(io::Error::other)?;
        let mut sc = quinn::ServerConfig::with_crypto(Arc::new(server));
        sc.transport_config(transport());
        let client = creds.client_config(ALPN)?;
        let client =
            quinn::crypto::rustls::QuicClientConfig::try_from(client).map_err(io::Error::other)?;
        let mut cc = quinn::ClientConfig::new(Arc::new(client));
        cc.transport_config(transport());
        // Port 0: the next free one of IDN's media range (peers connect in; F1-3).
        let sock = if cfg.port == 0 {
            idn_common::ports::media_udp(Ipv4Addr::UNSPECIFIED.into())?
        } else {
            std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, cfg.port))?
        };
        let port = sock.local_addr()?.port();
        let mut endpoint = {
            let _g = crate::quic::runtime().enter();
            quinn::Endpoint::new(
                quinn::EndpointConfig::default(),
                Some(sc),
                sock,
                Arc::new(quinn::TokioRuntime),
            )?
        };
        endpoint.set_default_client_config(cc);
        let ch = Arc::new_cyclic(|me| Channel {
            me: me.clone(),
            endpoint: endpoint.clone(),
            my_fp: cfg.enrollment.fingerprint(),
            enrollment: cfg.enrollment.clone(),
            handler,
            conns: Mutex::new(BTreeMap::new()),
            offers: Mutex::new(BTreeMap::new()),
            open_failed: Mutex::new(BTreeMap::new()),
            max_rate: AtomicU64::new(cfg.max_rate.unwrap_or(0)),
            cap: Arc::new(TokenBucket::default()),
            trouble: std::sync::atomic::AtomicU8::new(cfg.on_video_trouble.to_u8()),
            serving: Arc::new(tokio::sync::Semaphore::new(MAX_TRANSFERS)),
            status: Mutex::new(Status::default()),
            status_server: cfg.status_server.clone(),
            hashes: Mutex::new(BTreeMap::new()),
            announced: (!cfg.client_only).then_some(port),
        });
        if !cfg.client_only {
            cfg.enrollment.announce_endpoint(port);
        }
        cfg.enrollment.watch_directory();
        let weak = Arc::downgrade(&ch);
        crate::quic::runtime().spawn(accept_loop(endpoint, weak.clone()));
        crate::quic::runtime().spawn(reaper(weak));
        Ok(ch)
    }

    /// The UDP port the endpoint listens on.
    pub fn port(&self) -> u16 {
        self.endpoint.local_addr().map_or(0, |a| a.port())
    }

    /// The other devices with a channel endpoint, from the authority's directory.
    pub fn devices(&self) -> Vec<DirEntry> {
        self.enrollment
            .directory()
            .into_iter()
            .filter(|d| d.fingerprint != self.my_fp)
            .collect()
    }

    /// Sends a one-way message (§E.12.4). Returns once it is on its way.
    pub fn send(&self, to: &str, kind: &str, payload: &[u8]) -> io::Result<()> {
        check_type(kind)?;
        check_payload(payload)?;
        // To each of the device's endpoints this channel is connected with: the
        // application that knows the type takes it (§E.12.3).
        let msg = header(OP_MESSAGE, kind, payload);
        let mut last = None;
        let mut sent = false;
        for conn in self.connections(to)? {
            let msg = msg.clone();
            let used = conn.clone();
            let r = crate::quic::runtime().block_on(async move {
                let mut s = conn.open_uni().await.map_err(io::Error::other)?;
                s.write_all(&msg).await?;
                s.finish().map_err(io::Error::other)?;
                Ok::<_, io::Error>(())
            });
            match r {
                Ok(()) => {
                    sent = true;
                    self.touch(to, &used);
                }
                Err(e) => last = Some(e),
            }
        }
        match (sent, last) {
            (false, Some(e)) => Err(e),
            _ => Ok(()),
        }
    }

    /// Sends a request and waits up to `timeout` for the answer (§E.12.4). A device
    /// running more than one endpoint (its IDN Node's and a program's): the first of them
    /// that knows the type answers, its announced one first; [`Reply::Unknown`] only when
    /// none does (§E.12.3).
    pub fn request(
        &self,
        to: &str,
        kind: &str,
        payload: &[u8],
        timeout: Duration,
    ) -> io::Result<Reply> {
        check_type(kind)?;
        check_payload(payload)?;
        self.request_any(to, kind, payload, timeout, false)
    }

    /// [`request`](Self::request); `refused_next`: an endpoint's refusal also moves on to
    /// the next (a push: the Node refuses offers meant for a program beside it).
    fn request_any(
        &self,
        to: &str,
        kind: &str,
        payload: &[u8],
        timeout: Duration,
        refused_next: bool,
    ) -> io::Result<Reply> {
        let end = Instant::now() + timeout;
        let req = header(OP_REQUEST, kind, payload);
        let mut last: Option<io::Result<Reply>> = None;
        for conn in self.connections(to)? {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let used = conn.clone();
            let r = request_on(conn, &req, left);
            if r.is_ok() {
                self.touch(to, &used);
            }
            match r {
                Ok(Reply::Unknown) => {
                    if !matches!(last, Some(Ok(Reply::Refused))) {
                        last = Some(Ok(Reply::Unknown));
                    }
                }
                Ok(Reply::Refused) if refused_next => last = Some(Ok(Reply::Refused)),
                // (A connection that closed: the next endpoint, if any.)
                Err(e) if e.kind() != io::ErrorKind::TimedOut => {
                    if !matches!(last, Some(Ok(_))) {
                        last = Some(Err(e));
                    }
                }
                r => return r,
            }
        }
        last.unwrap_or_else(|| Err(io::Error::new(io::ErrorKind::TimedOut, "no answer in time")))
    }

    /// Fetches file `id` from device `from` into `sink`, resuming after what the sink
    /// already holds (§E.12.5). `progress` gets (bytes held, total) as data arrives.
    pub fn fetch(
        &self,
        from: &str,
        id: &str,
        sink: &mut dyn FileSink,
        progress: &mut dyn FnMut(u64, u64),
    ) -> io::Result<FileInfo> {
        self.fetch_via(None, from, id, None, sink, progress)
    }

    /// [`fetch`](Self::fetch), over `first` first (the connection a push came on): the
    /// device's endpoints in turn until one has the file.
    /// `offered`: a push's offer (its size and version): the file fetched must be that one.
    pub(super) fn fetch_via(
        &self,
        first: Option<quinn::Connection>,
        from: &str,
        id: &str,
        offered: Option<&FileInfo>,
        sink: &mut dyn FileSink,
        progress: &mut dyn FnMut(u64, u64),
    ) -> io::Result<FileInfo> {
        let mut conns: Vec<quinn::Connection> = first.into_iter().collect();
        match self.connections(from) {
            Ok(more) => {
                let seen: Vec<usize> = conns.iter().map(|c| c.stable_id()).collect();
                conns.extend(more.into_iter().filter(|c| !seen.contains(&c.stable_id())));
            }
            Err(e) if conns.is_empty() => return Err(e),
            Err(_) => {}
        }
        let mut last = None;
        for conn in conns {
            let used = conn.clone();
            let r = crate::quic::runtime().block_on(fetch_into(
                conn,
                id,
                offered,
                &mut *sink,
                &mut *progress,
            ));
            if r.is_ok() {
                self.touch(from, &used);
            }
            match r {
                // (Not there: the next endpoint; a cut: the next, saying the cut if none has it.)
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    last.get_or_insert(e);
                }
                Err(e) if is_closed(&e) => last = Some(e),
                r => return r,
            }
        }
        Err(last.unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such file")))
    }

    /// Offers the file at `path` to device `to` as `id` (a push, §E.12.5): if its
    /// application accepts, it fetches the file from this device. Returns its answer.
    pub fn push(&self, to: &str, id: &str, path: &Path, timeout: Duration) -> io::Result<Reply> {
        if id.is_empty() || id.len() > MAX_FILE_ID {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad file id"));
        }
        let info = self.file_info(id, path)?;
        // The file as it is now: a fetch must get this one, not what its path leads to
        // by then (a folder on the way swapped for a link; bug run 2026-11-24).
        let ident = file_identity(path);
        let mut offers = self.offers.lock_ok();
        offers.retain(|_, (_, at, _)| at.elapsed() < OFFER_LIFE);
        offers.insert(id.to_owned(), (path.to_owned(), Instant::now(), ident));
        drop(offers);
        let xml = file_element(&info).to_xml();
        // A device's endpoints in turn until one takes it (bug run 2026-11-24: the Node's
        // refusal ended it, and a program beside it was never offered the file).
        self.request_any(to, "idn.file.push", xml.as_bytes(), timeout, true)
    }

    /// Withdraws a file offered with [`Channel::push`].
    pub fn unoffer(&self, id: &str) {
        self.offers.lock_ok().remove(id);
    }

    /// Applies what the site sets and can change at once (`options/1`, §E.20):
    /// `channel.max_rate_mbps` and `channel.trouble`.
    pub fn apply_site(&self, site: &crate::trust::Resolved) {
        if let Some(r) = site_max_rate(site) {
            self.set_max_rate(r);
        }
        if let Some(t) = site_trouble(site) {
            self.set_trouble_response(t);
        }
    }

    /// Caps the file data this device sends, all transfers together, in bytes per
    /// second (50 MB/s: `Some(50_000_000)`); `None` removes the cap. Applies at once,
    /// also to transfers under way.
    pub fn set_max_rate(&self, bytes_per_second: Option<u64>) {
        self.max_rate
            .store(bytes_per_second.unwrap_or(0), Ordering::Relaxed);
    }

    /// The current cap, in bytes per second.
    pub fn max_rate(&self) -> Option<u64> {
        Some(self.max_rate.load(Ordering::Relaxed)).filter(|&r| r > 0)
    }

    /// Chooses how transfers react to live video trouble; applies at once.
    pub fn set_trouble_response(&self, r: TroubleResponse) {
        self.trouble.store(r.to_u8(), Ordering::Relaxed);
    }

    /// How transfers react to live video trouble.
    pub fn trouble_response(&self) -> TroubleResponse {
        TroubleResponse::from_u8(self.trouble.load(Ordering::Relaxed))
    }

    /// Paces one transfer to `peer` before it sends `n` more bytes: the shared cap,
    /// then this transfer's own slowdown while video trouble applies to it.
    pub(super) async fn pace(&self, p: &mut Pace, peer: &str, n: u64) {
        if let Some(cap) = self.max_rate() {
            self.cap.take(n, cap as f64).await;
        }
        let slow = match self.trouble_response() {
            TroubleResponse::Off => false,
            TroubleResponse::AllTransfers => trouble_with(None),
            TroubleResponse::PerDevice => trouble_with(Some(peer)),
        };
        p.next(n, slow).await;
    }

    /// Sets the status this device reports (§E.12.6): the driving application, what it
    /// is doing, and any number of named fields. Shown in the Discovery Server's
    /// monitoring panel as `app->activity`, and answered to `idn.status` requests.
    pub fn set_status(&self, app: &str, activity: &str, fields: &[(&str, &str)]) {
        let mut st = self.status.lock_ok();
        st.app = app.to_owned();
        st.activity = activity.to_owned();
        st.fields = fields
            .iter()
            .map(|(k, v)| (sanitize_field(k), (*v).to_owned()))
            .collect();
        let names: Vec<String> = st.fields.iter().map(|(k, _)| k.clone()).collect();
        // New field names need a new advertisement: properties are declared up front.
        if st.advertiser.is_none() || names.iter().any(|n| !st.declared.contains(n)) {
            st.advertiser = None;
            let server = self
                .status_server
                .as_deref()
                .unwrap_or(self.enrollment.discovery_server());
            if let Ok(server) = crate::discovery::ds::server_addr(server) {
                let props: Vec<String> = ["x-idn-app".to_owned(), "x-idn-activity".to_owned()]
                    .into_iter()
                    .chain(names.iter().map(|n| format!("x-idn-status-{n}")))
                    .collect();
                let extras: Vec<(&str, &str)> =
                    props.iter().map(|p| (p.as_str(), "string")).collect();
                st.advertiser = Some(crate::discovery::ds::Advertiser::receiver(
                    server,
                    self.enrollment.name(),
                    "metadata",
                    &extras,
                    false,
                ));
                st.declared = names;
            }
        }
        if let Some(a) = &st.advertiser {
            a.set("x-idn-app", &st.app);
            a.set("x-idn-activity", &st.activity);
            for (k, v) in &st.fields {
                a.set(&format!("x-idn-status-{k}"), v);
            }
        }
    }

    pub(super) fn status_xml(&self) -> String {
        let st = self.status.lock_ok();
        let mut e = Element::new("idn_status")
            .with_attr("app", &st.app)
            .with_attr("activity", &st.activity);
        for (k, v) in &st.fields {
            e.children.push(
                Element::new("field")
                    .with_attr("name", k)
                    .with_attr("value", v),
            );
        }
        e.to_xml()
    }
}
